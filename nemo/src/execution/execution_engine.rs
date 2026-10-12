//! Functionality which handles the execution of a program

use std::collections::{HashMap, HashSet};

use nemo_physical::{
    datavalues::AnyDataValue,
    dictionary::DvDict,
    management::database::sources::{SimpleTable, TableSource},
    meta::timing::TimedCode,
};

use crate::{
    error::{Error, report::ProgramReport, warned::Warned},
    execution::planning::{
        normalization::program::NormalizedProgram, strategy::forward::StrategyForward,
    },
    io::{formats::Export, import_manager::ImportManager},
    rule_file::RuleFile,
    rule_model::{
        components::tag::Tag, pipeline::transformations::default::TransformationDefault,
        programs::handle::ProgramHandle,
    },
    table_manager::{MemoryUsage, TableManager},
};

use super::{
    execution_parameters::ExecutionParameters, selection_strategy::strategy::RuleSelectionStrategy,
    tracing::rule_translation::RuleIdTranslation,
};

pub mod tracing;

// Number of tables that are periodically combined into one.
const MAX_FRAGMENTATION: usize = 8;

/// Stores useful information about a rule.
#[derive(Default, Debug, Copy, Clone)]
pub struct RuleInfo {
    /// The execution step this rule was last applied in.
    pub step_last_applied: usize,
}

impl RuleInfo {
    /// Create new [RuleInfo].
    pub fn new() -> Self {
        Self {
            step_last_applied: 0,
        }
    }
}

/// Object which handles the evaluation of the program.
#[derive(Debug)]
pub struct ExecutionEngine<RuleSelectionStrategy> {
    /// Handle to the logical program
    program_handle: ProgramHandle,

    /// Normalized program
    program: NormalizedProgram,

    /// Translation of rule ids between the normalized and the original program,
    /// used for tracing
    rule_translation: RuleIdTranslation,

    /// The picked selection strategy for rules
    selection_strategy: RuleSelectionStrategy,

    /// Management of tables that represent predicates
    table_manager: TableManager,
    /// Managermet of imports
    import_manager: ImportManager,

    /// Stores for each predicate the number of subtables
    predicate_fragmentation: HashMap<Tag, usize>,
    /// Stores for each predicate the step up until subtables have been combined
    predicate_last_union: HashMap<Tag, usize>,

    /// For each rule in `program` additional information
    rule_infos: Vec<RuleInfo>,
    /// For each step the rule index of the applied rule
    rule_history: Vec<usize>,
    /// For each step, the execution time in milliseconds
    step_times_ms: Vec<u128>,
    /// Current step
    current_step: usize,

    /// Predicates for which facts were added with [Self::add_facts]
    added_fact_predicates: HashSet<Tag>,
    /// Whether the engine has been executed or queried,
    /// after which no more facts can be added
    started: bool,
}

impl<Strategy: RuleSelectionStrategy> ExecutionEngine<Strategy> {
    /// Initialize a [ExecutionEngine] by parsing and translating
    /// the contents of the given file.
    pub async fn from_file(
        file: RuleFile,
        parameters: ExecutionParameters,
    ) -> Result<Warned<Self, ProgramReport>, Error> {
        let handle = ProgramHandle::from_file(&file);
        let report = ProgramReport::new(file);

        let (program, report) = report.merge_program_parser_report(handle)?;

        let (program, report) = report.merge_validation_report(
            &program,
            program.transform(TransformationDefault::new(&parameters)),
        )?;

        let engine = Self::from_handle(program, parameters).await?;

        report.warned(engine)
    }

    /// Initialize the [ExecutionEngine] from an already prepared [ProgramHandle]
    /// (for example one obtained from [crate::api::load_program_handle]).
    ///
    /// This performs **no** transformation.
    /// This is the entry point for callers that prepare
    /// the program themselves (e.g. applying their own transformations); the
    /// common case of going from a source file to an engine is covered by
    /// [Self::from_file], which prepares the program and then calls this.
    pub async fn from_handle(
        program: ProgramHandle,
        parameters: ExecutionParameters,
    ) -> Result<Self, Error> {
        Self::initialize(program, parameters.import_manager).await
    }

    /// Initialize the [ExecutionEngine].
    pub async fn initialize(
        program_handle: ProgramHandle,
        import_manager: ImportManager,
    ) -> Result<Self, Error> {
        let normalized_program = NormalizedProgram::normalize_program(&program_handle);
        let rule_translation = RuleIdTranslation::new(&program_handle, &normalized_program);

        let mut table_manager = TableManager::new();
        Self::register_all_predicates(&mut table_manager, &normalized_program);
        Self::add_all_constants(&mut table_manager, &normalized_program);
        Self::add_imports(&mut table_manager, &import_manager, &normalized_program).await?;

        let mut rule_infos = Vec::<RuleInfo>::new();
        normalized_program
            .rules()
            .iter()
            .for_each(|_| rule_infos.push(RuleInfo::new()));

        let selection_strategy = Strategy::new(normalized_program.rules().iter().collect())?;

        Ok(Self {
            program_handle,
            program: normalized_program,
            rule_translation,
            selection_strategy,
            table_manager,
            import_manager,
            predicate_fragmentation: HashMap::new(),
            predicate_last_union: HashMap::new(),
            rule_infos,
            rule_history: vec![usize::MAX], // Placeholder, Step counting starts at 1
            step_times_ms: vec![0],         // Placeholder, Step counting starts at 1
            current_step: 1,
            added_fact_predicates: HashSet::new(),
            started: false,
        })
    }

    /// Register all predicates found in a rule program to the [TableManager].
    fn register_all_predicates(table_manager: &mut TableManager, program: &NormalizedProgram) {
        for (predicate, arity) in program.predicates() {
            table_manager.register_predicate(predicate, arity);
        }
    }

    /// Add all constants appearing in the rules of the program to the dictionary.
    fn add_all_constants(table_manager: &mut TableManager, program: &NormalizedProgram) {
        for value in program.datavalues() {
            table_manager.dictionary_mut().add_datavalue(value.clone());
        }
    }

    /// Add edb tables to the [TableManager]
    /// based on the import declaration of the given progam.
    async fn add_imports(
        table_manager: &mut TableManager,
        import_manager: &ImportManager,
        program: &NormalizedProgram,
    ) -> Result<(), Error> {
        let mut predicate_to_sources = HashMap::<Tag, Vec<TableSource>>::new();

        // Add all the import specifications
        for import in program.imports() {
            let table_source = import_manager
                .table_provider_from_handler(&import.handler())
                .await?;

            predicate_to_sources
                .entry(import.predicate().clone())
                .or_default()
                .push(table_source);
        }

        // Add all the facts contained in the rule file as a source
        let mut predicate_to_rows = HashMap::<Tag, SimpleTable>::new();

        for fact in program.facts() {
            let table = predicate_to_rows
                .entry(fact.predicate())
                .or_insert(SimpleTable::new(fact.arity()));
            table.add_row(fact.datavalues().collect());
        }

        for (predicate, table) in predicate_to_rows.into_iter() {
            predicate_to_sources
                .entry(predicate)
                .or_default()
                .push(Box::new(table));
        }

        // Add all the sources to the table manager
        for (predicate, sources) in predicate_to_sources {
            table_manager.add_edb(predicate.clone(), sources);
        }

        Ok(())
    }

    /// Add input facts for a predicate, given as rows of [AnyDataValue]s,
    /// in addition to the facts and imports of the program.
    ///
    /// This avoids parsing facts as program text, which makes it the cheap way
    /// to load many facts produced by another program. Together with
    /// [Self::from_handle], it allows running a prepared program on different
    /// sets of facts. Rows are treated like facts of the program: duplicates
    /// are removed when the table is loaded.
    ///
    /// Predicates that do not appear in the program are registered with the
    /// arity of their rows, so that their facts can still be queried.
    ///
    /// # Errors
    /// Returns [Error::FactsAfterExecution] if the engine has already been
    /// executed or queried with [Self::predicate_rows] or [Self::predicate_rows_ids],
    /// and [Error::FactArity] if a row does not match the predicate's arity.
    pub fn add_facts<Rows>(&mut self, predicate: Tag, rows: Rows) -> Result<(), Error>
    where
        Rows: IntoIterator<Item = Vec<AnyDataValue>>,
    {
        if self.started {
            return Err(Error::FactsAfterExecution);
        }

        let mut rows = rows.into_iter().peekable();
        let Some(first) = rows.peek() else {
            return Ok(());
        };

        let known_arity = self.table_manager.predicate_arity(&predicate);
        let arity = known_arity.unwrap_or(first.len());

        let mut table = SimpleTable::new(arity);
        for row in rows {
            if row.len() != arity {
                return Err(Error::FactArity {
                    predicate: predicate.to_string(),
                    expected: arity,
                    found: row.len(),
                });
            }

            table.add_row(row);
        }

        if known_arity.is_none() {
            self.table_manager
                .register_predicate(predicate.clone(), arity);
        }
        self.table_manager.add_edb_table(predicate.clone(), table);
        self.added_fact_predicates.insert(predicate);

        Ok(())
    }

    async fn step(
        &mut self,
        rule_index: usize,
        execution: &StrategyForward,
    ) -> Result<Vec<Tag>, Error> {
        let timing_string = format!("Reasoning/Rules/Rule {rule_index}");

        TimedCode::instance().sub(&timing_string).start();
        log::info!(
            "<<< STEP {}: APPLYING RULE {} {} >>>",
            self.current_step,
            rule_index,
            self.program.rules()[rule_index]
        );

        self.rule_history.push(rule_index);

        let current_info = &mut self.rule_infos[rule_index];

        let updated_predicates = execution
            .execute(
                &mut self.table_manager,
                &self.import_manager,
                self.current_step,
                current_info.step_last_applied,
            )
            .await?;

        current_info.step_last_applied = self.current_step;

        let rule_duration = TimedCode::instance().sub(&timing_string).stop();

        self.step_times_ms.push(rule_duration.as_millis());

        log::info!("Rule duration: {} ms", rule_duration.as_millis());

        self.current_step += 1;

        Ok(updated_predicates)
    }

    async fn defrag(&mut self, updated_predicates: Vec<Tag>) -> Result<(), Error> {
        for updated_pred in updated_predicates {
            let counter = self
                .predicate_fragmentation
                .entry(updated_pred.clone())
                .or_insert(0);
            *counter += 1;

            if *counter == MAX_FRAGMENTATION {
                let start = if let Some(last_union) = self.predicate_last_union.get(&updated_pred) {
                    last_union + 1
                } else {
                    0
                };

                let range = start..(self.current_step + 1);

                self.table_manager
                    .combine_tables(&updated_pred, range)
                    .await?;

                self.predicate_last_union
                    .insert(updated_pred, self.current_step);

                *counter = 0;
            }
        }

        Ok(())
    }

    /// Executes the program.
    pub async fn execute(&mut self) -> Result<(), Error> {
        self.started = true;

        TimedCode::instance().sub("Reasoning/Rules").start();
        TimedCode::instance().sub("Reasoning/Execution").start();

        let predicates_with_facts = self
            .program
            .facts()
            .iter()
            .map(|atom| atom.predicate())
            .chain(self.added_fact_predicates.iter().cloned())
            .collect::<HashSet<_>>();

        let execution_strategy = self
            .program
            .rules()
            .iter()
            .enumerate()
            .map(|(rule_index, rule)| {
                StrategyForward::new(rule, rule_index, &predicates_with_facts)
            })
            .collect::<Vec<_>>();

        for (predicate, arity) in execution_strategy
            .iter()
            .flat_map(|strategy| strategy.special_predicates())
        {
            self.table_manager.register_predicate(predicate, arity);
        }

        for predicate in self.program.output_predicates().iter().cloned().chain(
            self.program
                .exports()
                .iter()
                .map(|export| export.predicate()),
        ) {
            self.table_manager.combine_predicate(&predicate).await?;
        }

        let mut new_derivations: Option<bool> = None;

        while let Some(index) = self.selection_strategy.next_rule(new_derivations) {
            let updated_predicates = self.step(index, &execution_strategy[index]).await?;
            new_derivations = Some(!updated_predicates.is_empty());

            self.defrag(updated_predicates).await?;
        }

        TimedCode::instance().sub("Reasoning/Rules").stop();
        TimedCode::instance().sub("Reasoning/Execution").stop();

        Ok(())
    }

    /// Get a reference to the loaded program.
    pub fn chase_program(&self) -> &NormalizedProgram {
        &self.program
    }

    /// Return the current [ProgramHandle].
    ///
    /// Note that this represents the transformed program and not the original.
    /// To obtain the original program, use [Self::original_program_handle].
    pub fn current_program_handle(&self) -> ProgramHandle {
        self.program_handle.clone()
    }

    /// Return the [ProgramHandle] of the original program, i.e. the program as
    /// it was created from the user input (the first revision of the pipeline).
    pub fn original_program_handle(&self) -> ProgramHandle {
        self.program_handle.original_revision()
    }

    /// Creates an [Iterator] over all facts of a predicate.
    pub async fn predicate_rows(
        &mut self,
        predicate: &Tag,
    ) -> Result<Option<impl Iterator<Item = Vec<AnyDataValue>> + '_>, Error> {
        self.started = true;

        let Some(table_id) = self.table_manager.combine_predicate(predicate).await? else {
            return Ok(None);
        };

        Ok(Some(self.table_manager.table_row_iterator(table_id).await?))
    }

    /// Creates an [Iterator] over all facts of a predicate with their corresponding id.
    pub async fn predicate_rows_ids(
        &mut self,
        predicate: &Tag,
    ) -> Result<Option<impl Iterator<Item = (Vec<AnyDataValue>, usize)>>, Error> {
        self.started = true;

        let Some(table_id) = self.table_manager.combine_predicate(predicate).await? else {
            return Ok(None);
        };

        // Once `combine_predicate` is called all subtables should be inmemory

        let iterator = self
            .table_manager
            .table_row_iterator_inmemory(table_id)?
            .map(|row| {
                let (id, _step) = self
                    .table_manager
                    .table_row_id_inmemory(predicate, &row)
                    .expect("row must exist since it comes from calling `combine_predicate`");
                (row, id)
            });

        Ok(Some(iterator))
    }

    /// Returns the arity of the predicate if the predicate is known to the engine,
    /// and `None` otherwise.
    pub fn predicate_arity(&self, predicate: &Tag) -> Option<usize> {
        self.program
            .predicate_arity(predicate)
            .or_else(|| self.table_manager.predicate_arity(predicate))
    }

    /// Return a list of all all export predicates and their respective [`Export`]s,
    /// which can be used for exporting into files.
    pub fn exports(&self) -> Vec<(Tag, Export)> {
        self.program
            .exports()
            .iter()
            .map(|export| (export.predicate(), export.handler()))
            .collect::<Vec<_>>()
    }

    /// Counts the facts of a single predicate that are currently in memory.
    pub fn count_facts_in_memory_for_predicate(&self, predicate: &Tag) -> Option<usize> {
        self.table_manager
            .count_rows_in_memory_for_predicate(predicate)
    }

    /// Count the number of facts of derived predicates that are currently in memory.
    pub fn count_facts_in_memory_for_derived_predicates(&self) -> usize {
        let output_predicates = self.program.output_predicates().iter().cloned();
        let export_predicates = self
            .program
            .exports()
            .iter()
            .map(|export| export.predicate());
        let derived_predicates = self.program.derived_predicates().iter().cloned();

        let predicates = output_predicates
            .chain(export_predicates)
            .chain(derived_predicates)
            .collect::<HashSet<_>>();

        predicates
            .iter()
            .map(|predicate| {
                self.count_facts_in_memory_for_predicate(predicate)
                    .unwrap_or(0)
            })
            .sum()
    }

    /// Return the amount of consumed memory for the tables used by the chase.
    pub fn memory_usage(&self) -> MemoryUsage {
        self.table_manager.memory_usage()
    }

    /// For a given iterator over rule execution steps
    /// returns the sum of execution time for those steps.
    pub fn steps_time_ms<Iter: Iterator<Item = usize>>(&self, steps: Iter) -> u128 {
        steps
            .map(|step| self.step_times_ms.get(step).cloned().unwrap_or_default())
            .sum()
    }
}

#[cfg(test)]
mod test {
    use std::{assert_matches, default::Default};

    use test_log;
    use tokio;

    use nemo_physical::datavalues::AnyDataValue;

    use crate::{
        error::Error,
        execution::{DefaultExecutionEngine, execution_parameters::ExecutionParameters},
        rule_file::RuleFile,
        rule_model::{
            components::tag::Tag, pipeline::transformations::default::TransformationDefault,
            programs::handle::ProgramHandle,
        },
    };

    #[tokio::test]
    #[cfg_attr(miri, ignore)]
    async fn rule_source_position_reports_line() {
        // A fact on line 1 and a rule on line 2.
        let source = "a(1).
b(?x) :- a(?x).";

        let file = RuleFile::new(source.to_string(), "test".to_string());

        let engine = DefaultExecutionEngine::from_file(file, ExecutionParameters::default())
            .await
            .unwrap()
            .into_object();

        // The frontend resolves a tracing rule id via the original program handle.
        let original = engine.original_program_handle();

        // Rule id 0 is the first (and only) rule, which is on line 2, column 1.
        let rule = original.rule_by_index(0).expect("rule id is valid");
        let position = original
            .source_position(rule)
            .expect("rule originates from the source");
        assert_eq!(position.line, 2);
        assert_eq!(position.column, 1);
    }

    fn prepare(source: &str) -> ProgramHandle {
        let file = RuleFile::new(source.to_string(), "test".to_string());
        ProgramHandle::from_file(&file)
            .expect("program parses")
            .into_object()
            .transform(TransformationDefault::new(&ExecutionParameters::default()))
            .expect("program is valid")
    }

    async fn sorted_rows(engine: &mut DefaultExecutionEngine, predicate: &str) -> Vec<String> {
        let mut rows = engine
            .predicate_rows(&Tag::from(predicate))
            .await
            .unwrap()
            .map(|rows| rows.map(|row| format!("{row:?}")).collect::<Vec<_>>())
            .unwrap_or_default();
        rows.sort();
        rows
    }

    const RULES: &str = "b(?x, ?y) :- a(?x, ?y).
c(?x) :- a(?x, 1), ~d(?x).
@output b.
@output c.";

    #[tokio::test]
    #[cfg_attr(miri, ignore)]
    async fn add_facts_matches_text_facts() {
        let text =
            format!("{RULES}\na(\"s\", 1). a(t, 2.5). a(3, \"u\"). a(\"s\", 1). a(true, 1). d(3).");
        let mut from_text =
            DefaultExecutionEngine::from_handle(prepare(&text), ExecutionParameters::default())
                .await
                .unwrap();
        from_text.execute().await.unwrap();

        let mut from_rows =
            DefaultExecutionEngine::from_handle(prepare(RULES), ExecutionParameters::default())
                .await
                .unwrap();
        from_rows
            .add_facts(
                Tag::from("a"),
                vec![
                    vec![
                        AnyDataValue::new_plain_string("s".to_string()),
                        AnyDataValue::new_integer_from_i64(1),
                    ],
                    vec![
                        AnyDataValue::new_iri("t".to_string()),
                        AnyDataValue::new_double_from_f64(2.5).unwrap(),
                    ],
                    vec![
                        AnyDataValue::new_integer_from_i64(3),
                        AnyDataValue::new_plain_string("u".to_string()),
                    ],
                    vec![
                        AnyDataValue::new_plain_string("s".to_string()),
                        AnyDataValue::new_integer_from_i64(1),
                    ],
                    vec![
                        AnyDataValue::new_boolean(true),
                        AnyDataValue::new_integer_from_i64(1),
                    ],
                ],
            )
            .unwrap();
        from_rows
            .add_facts(
                Tag::from("d"),
                vec![vec![AnyDataValue::new_integer_from_i64(3)]],
            )
            .unwrap();
        from_rows.execute().await.unwrap();

        for predicate in ["a", "b", "c", "d"] {
            let expected = sorted_rows(&mut from_text, predicate).await;
            assert!(!expected.is_empty());
            assert_eq!(sorted_rows(&mut from_rows, predicate).await, expected);
        }
        assert_eq!(sorted_rows(&mut from_rows, "b").await.len(), 4);
        assert_eq!(sorted_rows(&mut from_rows, "c").await.len(), 2);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)]
    async fn add_facts_extends_program_facts() {
        let program = prepare(&format!("{RULES}\na(1, 1)."));
        let mut engine =
            DefaultExecutionEngine::from_handle(program, ExecutionParameters::default())
                .await
                .unwrap();
        engine
            .add_facts(
                Tag::from("a"),
                vec![vec![
                    AnyDataValue::new_integer_from_i64(2),
                    AnyDataValue::new_integer_from_i64(1),
                ]],
            )
            .unwrap();
        engine
            .add_facts(
                Tag::from("a"),
                vec![vec![
                    AnyDataValue::new_integer_from_i64(3),
                    AnyDataValue::new_integer_from_i64(1),
                ]],
            )
            .unwrap();
        engine.execute().await.unwrap();

        assert_eq!(sorted_rows(&mut engine, "c").await.len(), 3);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)]
    async fn add_facts_registers_unknown_predicates() {
        let mut engine =
            DefaultExecutionEngine::from_handle(prepare(RULES), ExecutionParameters::default())
                .await
                .unwrap();
        engine
            .add_facts(
                Tag::from("unused"),
                vec![vec![AnyDataValue::new_integer_from_i64(1)]],
            )
            .unwrap();
        engine.execute().await.unwrap();

        assert_eq!(engine.predicate_arity(&Tag::from("unused")), Some(1));
        assert_eq!(sorted_rows(&mut engine, "unused").await.len(), 1);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)]
    async fn add_facts_rejects_invalid_input() {
        let mut engine =
            DefaultExecutionEngine::from_handle(prepare(RULES), ExecutionParameters::default())
                .await
                .unwrap();

        let wrong_arity = engine.add_facts(
            Tag::from("a"),
            vec![vec![AnyDataValue::new_integer_from_i64(1)]],
        );
        assert_matches!(
            wrong_arity,
            Err(Error::FactArity {
                expected: 2,
                found: 1,
                ..
            })
        );

        engine.execute().await.unwrap();
        let after_execution = engine.add_facts(
            Tag::from("a"),
            vec![vec![
                AnyDataValue::new_integer_from_i64(1),
                AnyDataValue::new_integer_from_i64(1),
            ]],
        );
        assert_matches!(after_execution, Err(Error::FactsAfterExecution));
    }

    #[tokio::test]
    #[test_log::test]
    #[cfg_attr(miri, ignore)]
    async fn issue_759() {
        const ITERATIONS: usize = 32_768;

        let file = RuleFile::new("foo(bar).".to_string(), Default::default());
        let parameters = ExecutionParameters::default();
        let program = ProgramHandle::from_file(&file)
            .expect("program parses")
            .into_object()
            .transform(TransformationDefault::new(&parameters))
            .expect("program is valid");

        for _ in 1..=ITERATIONS {
            let engine = DefaultExecutionEngine::from_handle(
                program.clone(),
                ExecutionParameters::default(),
            )
            .await;
            assert_matches!(engine, Ok(_));
        }
    }
}
