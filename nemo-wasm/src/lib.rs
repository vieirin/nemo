#![feature(alloc_error_hook)]

use std::{alloc::Layout, collections::HashMap, io::Cursor, rc::Rc};

use gloo_utils::format::JsValueSerdeExt;
use indexmap::{IndexMap, IndexSet};
use js_sys::{Array, ArrayBuffer, Object, Reflect, Set, Uint8Array};
use thiserror::Error;
use wasm_bindgen::{JsCast, JsValue, prelude::wasm_bindgen};
use wasm_bindgen_futures::JsFuture;
use web_sys::Blob;

use nemo::{
    datavalues::{AnyDataValue, DataValue},
    error::report::ProgramReport,
    execution::{DefaultExecutionEngine, DefaultPreparedPlan, ExecutionEngine},
    io::{
        ImportManager,
        resource_providers::{ResourceProvider, ResourceProviders, http},
    },
    rule_model::components::tag::Tag,
};

use nemo_physical::resource::Resource;

#[cfg(feature = "language-server")]
mod language_server;
mod models;

#[derive(Error, Debug)]
enum WasmOrInternalNemoError {
    /// Nemo-internal error
    #[error(transparent)]
    Nemo(#[from] nemo::error::Error),
    #[error("Invalid program:\n {0}")]
    Program(String),
    #[error("Internal reflection error: {0:#?}")]
    Reflection(JsValue),
    #[error("Invalid fact for predicate `{predicate}`: {message}")]
    Fact { predicate: String, message: String },
    #[error("Invalid resource `{resource}`: {message}")]
    Resource { resource: String, message: String },
}

impl From<nemo::error::Error> for NemoError {
    fn from(error: nemo::error::Error) -> Self {
        match error {
            nemo::error::Error::ProgramReport(report) => report.into(),
            error => NemoError(WasmOrInternalNemoError::Nemo(error)),
        }
    }
}

impl From<ProgramReport> for NemoError {
    fn from(report: ProgramReport) -> Self {
        // This is cursed. The report only allows writing the proper error message to
        // std::io::Write but not std::fmt::Write. (Strings only implement the latter...)
        let mut bytes: Vec<u8> = vec![];
        report
            .write(&mut bytes)
            .expect("We should always be able to write to a Vec<u8>.");
        let string = String::from_utf8_lossy(&bytes);
        NemoError(WasmOrInternalNemoError::Program(string.into_owned()))
    }
}

#[wasm_bindgen]
#[derive(Debug)]
#[allow(dead_code)]
pub struct NemoError(WasmOrInternalNemoError);

#[wasm_bindgen]
impl NemoError {
    #[allow(clippy::inherent_to_string)]
    #[wasm_bindgen(js_name = "toString")]
    pub fn to_string(&self) -> String {
        format!("{}", self.0)
    }
}

#[wasm_bindgen]
pub struct NemoResource {
    accept: String,
    url: String,
}

#[wasm_bindgen]
impl NemoResource {
    pub fn accept(&self) -> String {
        self.accept.clone()
    }

    pub fn url(&self) -> String {
        self.url.clone()
    }
}

/// Resources given to an engine from JavaScript, read into memory.
///
/// Unlike reading [Blob]s with `FileReaderSync`, this also works
/// outside of web workers, e.g. in Node.js.
#[derive(Debug)]
pub struct BlobResourceProvider {
    resources: HashMap<String, Rc<[u8]>>,
}

impl BlobResourceProvider {
    /// Read resources from a JavaScript object that maps resource names to
    /// their contents, given as `Blob`, `Uint8Array`, `ArrayBuffer` or string.
    /// `undefined` and `null` are read as no resources.
    pub async fn from_js(resources: Option<&JsValue>) -> Result<Self, NemoError> {
        let mut result = HashMap::new();

        let Some(resources) = resources.filter(|resources| !resources.is_null()) else {
            return Ok(Self { resources: result });
        };

        for key in Reflect::own_keys(resources)
            .map_err(WasmOrInternalNemoError::Reflection)
            .map_err(NemoError)?
        {
            let Some(resource) = key.as_string() else {
                continue;
            };
            let value = Reflect::get(resources, &key)
                .map_err(WasmOrInternalNemoError::Reflection)
                .map_err(NemoError)?;
            let bytes = Self::read(value).await.map_err(|message| {
                NemoError(WasmOrInternalNemoError::Resource {
                    resource: resource.clone(),
                    message,
                })
            })?;

            result.insert(resource, bytes);
        }

        Ok(Self { resources: result })
    }

    async fn read(value: JsValue) -> Result<Rc<[u8]>, String> {
        if let Some(text) = value.as_string() {
            return Ok(text.into_bytes().into());
        }
        if let Some(array) = value.dyn_ref::<Uint8Array>() {
            return Ok(array.to_vec().into());
        }
        if let Some(buffer) = value.dyn_ref::<ArrayBuffer>() {
            return Ok(Uint8Array::new(buffer).to_vec().into());
        }
        if let Some(blob) = value.dyn_ref::<Blob>() {
            let buffer = JsFuture::from(blob.array_buffer())
                .await
                .map_err(|error| format!("{error:?}"))?;
            return Ok(Uint8Array::new(&buffer).to_vec().into());
        }

        Err("expected a Blob, Uint8Array, ArrayBuffer or string".to_string())
    }

    /// Return the [ResourceProviders] for these resources, falling back to HTTP.
    fn into_providers(self) -> ResourceProviders {
        if self.resources.is_empty() {
            ResourceProviders::from(vec![Box::<http::HttpResourceProvider>::default()])
        } else {
            ResourceProviders::from(vec![
                Box::new(self),
                Box::<http::HttpResourceProvider>::default(),
            ])
        }
    }
}

#[async_trait::async_trait(?Send)]
impl ResourceProvider for BlobResourceProvider {
    async fn open_resource(
        &self,
        resource: &Resource,
        _media_type: &str,
    ) -> Result<Option<Box<dyn std::io::Read>>, nemo_physical::error::ReadingError> {
        Ok(self
            .resources
            .get(&resource.to_string())
            .map(|bytes| Box::new(Cursor::new(bytes.clone())) as Box<dyn std::io::Read>))
    }
}

/// A program that has been parsed, validated and transformed once,
/// from which any number of engines can be created with `createEngine`.
///
/// Creating engines from a prepared program avoids parsing and planning the
/// rules again for each engine. Combined with `NemoEngine.addFacts`, this runs
/// the same rules on different facts without parsing anything.
#[wasm_bindgen]
pub struct NemoProgram {
    plan: Rc<DefaultPreparedPlan>,
}

#[wasm_bindgen]
impl NemoProgram {
    /// Parse, validate and transform the program given as text,
    /// and plan the evaluation of its rules.
    #[wasm_bindgen(constructor)]
    pub fn new(input: &str) -> Result<NemoProgram, NemoError> {
        let program =
            nemo::api::load_program_handle(input.to_string(), "NemoWasmFile".to_string())?;
        let plan = DefaultPreparedPlan::new(program)?;

        Ok(NemoProgram {
            plan: Rc::new(plan),
        })
    }

    /// Create an engine for this program.
    ///
    /// `resources` optionally maps resource names used in imports to their contents,
    /// see `NemoEngine.new`.
    #[wasm_bindgen(js_name = "createEngine")]
    pub async fn create_engine(
        &self,
        resources: Option<NemoResources>,
    ) -> Result<NemoEngine, NemoError> {
        let providers = BlobResourceProvider::from_js(resources.as_deref())
            .await?
            .into_providers();

        NemoEngine::from_plan(self.plan.clone(), providers).await
    }
}

#[wasm_bindgen]
extern "C" {
    /// Contents of resources used in imports, by resource name.
    #[wasm_bindgen(typescript_type = "NemoResources")]
    pub type NemoResources;
}

#[wasm_bindgen(typescript_custom_section)]
const TS_TYPES: &'static str = r#"
/**
 * Contents of resources used in imports, by resource name.
 */
export type NemoResources = Record<string, Blob | Uint8Array | ArrayBuffer | string>;

/**
 * A value of a fact given to `NemoEngine.addFacts`:
 * strings are plain strings (`"abc"` in Nemo),
 * safe integers and bigints are integers, other numbers are doubles,
 * and `{ iri: "abc" }` is the constant `abc`.
 */
export type NemoFactValue = string | number | bigint | boolean | { iri: string };

/**
 * A value of a result row.
 */
export type NemoValue = string | number | bigint | boolean;
"#;

/// An engine that evaluates a program on its facts.
///
/// Engines hold a lot of memory: release them with `free()`
/// (or `using`, as they implement `Symbol.dispose`) when they are no longer needed.
#[wasm_bindgen]
pub struct NemoEngine {
    engine: DefaultExecutionEngine,
    /// The plan of the program, to create the engine again when facts are replaced
    plan: Rc<DefaultPreparedPlan>,
    /// Resources for imports, to create the engine again
    resource_providers: ResourceProviders,
    /// Facts added with `addFacts` or `setFacts`, by predicate
    facts: IndexMap<Tag, Vec<Vec<AnyDataValue>>>,
    /// Whether `engine` no longer reflects `facts` and must be created again
    stale: bool,
}

#[cfg(feature = "web_sys_unstable_apis")]
fn std_io_error_from_js_value(js_value: JsValue, prefix: &str) -> std::io::Error {
    std::io::Error::other(format!("{prefix}: {js_value:#?}"))
}

#[cfg(feature = "web_sys_unstable_apis")]
struct SyncAccessHandleWriter(web_sys::FileSystemSyncAccessHandle);

#[cfg(feature = "web_sys_unstable_apis")]
impl std::io::Write for SyncAccessHandleWriter {
    fn write(&mut self, buf: &[u8]) -> Result<usize, std::io::Error> {
        let buf: Vec<_> = buf.into();
        let bytes_written = self.0.write_with_u8_array(&buf).map_err(|js_value| {
            std_io_error_from_js_value(
                js_value,
                "Error while writing to FileSystemSyncAccessHandle",
            )
        })?;

        // Convert to usize safely
        let converted_bytes_written = bytes_written as usize;
        if converted_bytes_written as f64 != bytes_written {
            return Err(std::io::Error::other(format!(
                "Error while converting number of bytes written to usize: {bytes_written:#?}"
            )));
        }

        Ok(converted_bytes_written)
    }
    fn flush(&mut self) -> Result<(), std::io::Error> {
        self.0.flush().map_err(|js_value| {
            std_io_error_from_js_value(js_value, "Error while flushing FileSystemSyncAccessHandle")
        })?;

        Ok(())
    }
}

#[wasm_bindgen]
impl NemoEngine {
    // Not using wasm_bindgen(constructor) here since this does not seem
    // to generate the right types for async functions.
    /// Parse the program given as text and create an engine for it.
    ///
    /// `resources` optionally maps resource names used in imports to their contents,
    /// given as `Blob`, `Uint8Array`, `ArrayBuffer` or string.
    ///
    /// To create many engines for the same rules, prepare them once with `NemoProgram`.
    #[wasm_bindgen]
    pub async fn new(
        input: &str,
        resources: Option<NemoResources>,
    ) -> Result<NemoEngine, NemoError> {
        let resource_providers = BlobResourceProvider::from_js(resources.as_deref())
            .await?
            .into_providers();

        let program = NemoProgram::new(input)?;

        Self::from_plan(program.plan, resource_providers).await
    }

    async fn from_plan(
        plan: Rc<DefaultPreparedPlan>,
        resource_providers: ResourceProviders,
    ) -> Result<NemoEngine, NemoError> {
        let engine = Self::create_engine(&plan, &resource_providers).await?;

        Ok(NemoEngine {
            engine,
            plan,
            resource_providers,
            facts: IndexMap::new(),
            stale: false,
        })
    }

    async fn create_engine(
        plan: &DefaultPreparedPlan,
        resource_providers: &ResourceProviders,
    ) -> Result<DefaultExecutionEngine, NemoError> {
        let import_manager = ImportManager::new(resource_providers.clone());

        Ok(ExecutionEngine::from_plan(plan, import_manager).await?)
    }

    /// Create the engine again if facts have changed since it was used.
    async fn refresh(&mut self) -> Result<(), NemoError> {
        if !self.stale {
            return Ok(());
        }

        let mut engine = Self::create_engine(&self.plan, &self.resource_providers).await?;
        for (predicate, rows) in &self.facts {
            engine.add_facts(predicate.clone(), rows.iter().cloned())?;
        }

        self.engine = engine;
        self.stale = false;

        Ok(())
    }

    /// Convert rows of JavaScript values into rows of [AnyDataValue]s,
    /// checking that they match the arity of the predicate.
    fn rows_from_js(
        &self,
        predicate: &Tag,
        rows: &Array,
    ) -> Result<Vec<Vec<AnyDataValue>>, NemoError> {
        let error = |message: String| {
            NemoError(WasmOrInternalNemoError::Fact {
                predicate: predicate.to_string(),
                message,
            })
        };

        let mut arity = self
            .engine
            .predicate_arity(predicate)
            .or_else(|| self.facts.get(predicate)?.first().map(Vec::len));

        let mut result = Vec::with_capacity(rows.length() as usize);
        for row in rows.iter() {
            let row: Array = row
                .dyn_into()
                .map_err(|_| error("expected each row to be an array".to_string()))?;

            let length = row.length() as usize;
            let expected = *arity.get_or_insert(length);
            if length != expected {
                return Err(error(format!(
                    "row has {length} values, but the predicate has arity {expected}"
                )));
            }

            let values = row
                .iter()
                .map(|value| datavalue_from_js(&value).map_err(error))
                .collect::<Result<Vec<_>, _>>()?;
            result.push(values);
        }

        Ok(result)
    }

    /// Add facts for a predicate, given as an array of rows of values,
    /// without parsing them as program text.
    ///
    /// The facts are used together with the facts of the program.
    /// Strings become plain strings (like `"abc"` in Nemo), safe integers and bigints
    /// become integers, other numbers become doubles, booleans become booleans,
    /// and `{ iri: "abc" }` becomes the constant `abc`.
    ///
    /// Adding facts after `reason` makes the next call to `reason` start over
    /// from the program and all the facts added so far.
    #[wasm_bindgen(js_name = "addFacts")]
    pub fn add_facts(
        &mut self,
        predicate: String,
        #[wasm_bindgen(unchecked_param_type = "NemoFactValue[][]")] rows: Array,
    ) -> Result<(), NemoError> {
        let predicate = Tag::from(predicate);
        let rows = self.rows_from_js(&predicate, &rows)?;

        if !self.stale {
            match self
                .engine
                .add_facts(predicate.clone(), rows.iter().cloned())
            {
                Ok(()) => {}
                Err(nemo::error::Error::FactsAfterExecution) => self.stale = true,
                Err(error) => return Err(error.into()),
            }
        }

        self.facts.entry(predicate).or_default().extend(rows);

        Ok(())
    }

    /// Replace the facts of a predicate that were added with `addFacts` or `setFacts`;
    /// facts written in the program are kept.
    ///
    /// The next call to `reason` starts over from the program
    /// and the current facts. The program is not parsed again.
    #[wasm_bindgen(js_name = "setFacts")]
    pub fn set_facts(
        &mut self,
        predicate: String,
        #[wasm_bindgen(unchecked_param_type = "NemoFactValue[][]")] rows: Array,
    ) -> Result<(), NemoError> {
        let predicate = Tag::from(predicate);
        let rows = self.rows_from_js(&predicate, &rows)?;

        self.facts.insert(predicate, rows);
        self.stale = true;

        Ok(())
    }

    #[wasm_bindgen(js_name = "getOutputPredicates")]
    pub fn output_predicates(&self) -> Array {
        let target_predicates: IndexSet<_> = self
            .engine
            .chase_program()
            .output_predicates()
            .iter()
            .map(|o| o.to_string())
            .chain(
                self.engine
                    .chase_program()
                    .exports()
                    .iter()
                    .map(|o| o.predicate().to_string()),
            )
            .collect();

        target_predicates.into_iter().map(JsValue::from).collect()
    }

    #[wasm_bindgen(js_name = "getEDBPredicates")]
    pub fn edb_predicates(&self) -> Set {
        let js_set = Set::new(&JsValue::undefined());

        for tag in self.engine.chase_program().import_predicates() {
            js_set.add(&JsValue::from(tag.to_string()));
        }

        js_set
    }

    #[wasm_bindgen(js_name = "getLineNumberFromRuleId")]
    pub fn line_number_from_rule_id(&self, rule_id: usize) -> Option<u32> {
        let prog_handle = self.engine.original_program_handle();

        let rule = prog_handle.rule_by_index(rule_id)?;
        let position = prog_handle
            .source_position(rule)
            .expect("rule originates from the source");

        Some(position.line)
    }

    #[wasm_bindgen]
    pub async fn reason(&mut self) -> Result<(), NemoError> {
        self.refresh().await?;
        self.engine
            .execute()
            .await
            .map_err(WasmOrInternalNemoError::Nemo)
            .map_err(NemoError)
    }

    #[wasm_bindgen(js_name = "countFactsInMemoryForDerivedPredicates")]
    pub fn count_facts_in_memory_for_derived_predicates(&mut self) -> usize {
        self.engine.count_facts_in_memory_for_derived_predicates()
    }

    #[wasm_bindgen(js_name = "countFactsInMemoryForPredicate")]
    pub fn count_facts_in_memory_for_predicate(&mut self, predicate: String) -> Option<usize> {
        self.engine
            .count_facts_in_memory_for_predicate(&predicate.into())
    }

    #[wasm_bindgen(js_name = "getResult")]
    pub async fn result(&mut self, predicate: String) -> Result<NemoResults, NemoError> {
        self.refresh().await?;
        let pred_tag = Tag::from(predicate);
        let iter = self
            .engine
            .predicate_rows_ids(&pred_tag)
            .await
            .map_err(WasmOrInternalNemoError::Nemo)
            .map_err(NemoError)?;

        let results = NemoResults(Box::new(
            iter.into_iter().flatten().collect::<Vec<_>>().into_iter(),
        ));

        Ok(results)
    }

    /// Return all rows of a predicate as an array of arrays of values.
    ///
    /// This is a faster alternative to iterating over `getResult`
    /// when the row ids are not needed.
    #[wasm_bindgen(js_name = "getResultRows", unchecked_return_type = "NemoValue[][]")]
    pub async fn result_rows(&mut self, predicate: String) -> Result<Array, NemoError> {
        self.refresh().await?;
        let rows = self
            .engine
            .predicate_rows(&Tag::from(predicate))
            .await
            .map_err(WasmOrInternalNemoError::Nemo)
            .map_err(NemoError)?;

        Ok(rows
            .into_iter()
            .flatten()
            .map(|row| JsValue::from(row_to_js(row)))
            .collect())
    }

    #[cfg(feature = "web_sys_unstable_apis")]
    #[wasm_bindgen(js_name = "savePredicate")]
    pub async fn write_result_to_sync_access_handle(
        &mut self,
        predicate: String,
        sync_access_handle: web_sys::FileSystemSyncAccessHandle,
    ) -> Result<(), NemoError> {
        use nemo::io::{ExportManager, formats::dsv::DsvHandler};

        self.refresh().await?;
        let identifier = Tag::from(predicate.clone());

        let Some(arity) = self.engine.predicate_arity(&identifier) else {
            return Ok(());
        };

        let Some(record_iter) = self
            .engine
            .predicate_rows(&identifier)
            .await
            .map_err(WasmOrInternalNemoError::Nemo)
            .map_err(NemoError)?
        else {
            return Ok(());
        };

        let writer = SyncAccessHandleWriter(sync_access_handle);

        let export_handler: DsvHandler = DsvHandler::new(b',', arity);
        let export_manager: ExportManager = Default::default();

        export_manager
            .export_table_with_writer(Box::new(writer), &export_handler, Some(record_iter))
            .map_err(WasmOrInternalNemoError::Nemo)
            .map_err(NemoError)
    }

    #[wasm_bindgen(js_name = "traceTreeForTable")]
    pub async fn trace_tree_for_table(
        &mut self,
        tree_for_table_query: JsValue,
    ) -> Result<JsValue, NemoError> {
        self.refresh().await?;
        let tree_for_table_query: models::TreeForTableQuery = tree_for_table_query
            .into_serde()
            .map_err(|err| WasmOrInternalNemoError::Reflection(err.to_string().into()))
            .map_err(NemoError)?;

        let query =
            nemo::execution::tracing::tree_query::TreeForTableQuery::from(tree_for_table_query);

        let response = self
            .engine
            .trace_tree(query)
            .await
            .map_err(WasmOrInternalNemoError::Nemo)
            .map_err(NemoError)?;

        let tree_for_table_response = models::TreeForTableResponse::from(response);

        JsValue::from_serde(&tree_for_table_response)
            .map_err(|err| WasmOrInternalNemoError::Reflection(err.to_string().into()))
            .map_err(NemoError)
    }

    #[wasm_bindgen(js_name = "traceTableEntriesForTreeNodes")]
    pub async fn trace_table_entries_for_tree_nodes(
        &mut self,
        table_entries_for_tree_nodes_query: JsValue,
    ) -> Result<JsValue, JsValue> {
        self.refresh().await?;
        let table_entries_for_tree_nodes_query: models::TableEntriesForTreeNodesQuery =
            table_entries_for_tree_nodes_query
                .into_serde()
                .map_err(|err| WasmOrInternalNemoError::Reflection(err.to_string().into()))
                .map_err(NemoError)?;

        let query = nemo::execution::tracing::node_query::TableEntriesForTreeNodesQuery::from(
            table_entries_for_tree_nodes_query,
        );

        let response = self
            .engine
            .trace_node(&query)
            .await
            .map_err(WasmOrInternalNemoError::Nemo)
            .map_err(NemoError)?;

        let table_entries_for_tree_nodes_response = response
            .elements
            .into_iter()
            .map(models::TableEntriesForTreeNodesResponseInner::from)
            .collect::<Vec<_>>();

        Ok(JsValue::from_serde(&table_entries_for_tree_nodes_response)
            .map_err(|err| WasmOrInternalNemoError::Reflection(err.to_string().into()))
            .map_err(NemoError)?)
    }
}

#[wasm_bindgen]
pub struct NemoResults(Box<dyn Iterator<Item = (Vec<AnyDataValue>, usize)> + Send>);

#[wasm_bindgen]
#[derive(Clone)]
pub struct NemoResultRow {
    #[wasm_bindgen(getter_with_clone)]
    pub values: Array,
    pub id: usize,
}

#[wasm_bindgen]
pub struct NemoResultsIteratorNext {
    pub done: bool,
    #[wasm_bindgen(getter_with_clone)]
    pub value: Option<NemoResultRow>,
}

#[wasm_bindgen]
impl NemoResults {
    #[allow(clippy::should_implement_trait)]
    #[wasm_bindgen]
    pub fn next(&mut self) -> NemoResultsIteratorNext {
        match self.0.next() {
            Some((row, id)) => NemoResultsIteratorNext {
                done: false,
                value: Some(NemoResultRow {
                    values: row_to_js(row),
                    id,
                }),
            },
            None => NemoResultsIteratorNext {
                done: true,
                value: None,
            },
        }
    }

    /// Make the results iterable with `for...of`.
    ///
    /// The rows are plain objects with the same `values` and `id` as the
    /// `NemoResultRow`s returned by `next`.
    #[wasm_bindgen(
        js_name = "[Symbol.iterator]",
        unchecked_return_type = "IterableIterator<{ values: NemoValue[], id: number }>"
    )]
    pub fn iterator(&mut self) -> JsValue {
        let rows: Array = self
            .0
            .by_ref()
            .map(|(row, id)| {
                let object = Object::new();
                Reflect::set(&object, &"values".into(), &row_to_js(row))
                    .expect("setting a property of a new object succeeds");
                Reflect::set(&object, &"id".into(), &JsValue::from(id))
                    .expect("setting a property of a new object succeeds");
                JsValue::from(object)
            })
            .collect();

        rows.values().into()
    }
}

/// Convert a JavaScript value of a fact into an [AnyDataValue].
fn datavalue_from_js(value: &JsValue) -> Result<AnyDataValue, String> {
    if let Some(string) = value.as_string() {
        return Ok(AnyDataValue::new_plain_string(string));
    }
    if let Some(number) = value.as_f64() {
        const MAX_SAFE_INTEGER: f64 = 9007199254740991.0;
        if number.fract() == 0.0 && number.abs() <= MAX_SAFE_INTEGER {
            return Ok(AnyDataValue::new_integer_from_i64(number as i64));
        }
        return AnyDataValue::new_double_from_f64(number).map_err(|error| error.to_string());
    }
    if let Some(boolean) = value.as_bool() {
        return Ok(AnyDataValue::new_boolean(boolean));
    }
    if value.is_bigint() {
        return i64::try_from(value.clone())
            .map(AnyDataValue::new_integer_from_i64)
            .map_err(|_| "bigint does not fit into 64 bits".to_string());
    }
    if value.is_object()
        && let Some(iri) = Reflect::get(value, &"iri".into())
            .ok()
            .and_then(|iri| iri.as_string())
    {
        return Ok(AnyDataValue::new_iri(iri));
    }

    Err(format!(
        "expected a string, number, bigint, boolean or {{ iri: string }}, found {value:?}"
    ))
}

/// Convert a row of [AnyDataValue]s into an array of JavaScript values.
fn row_to_js(row: Vec<AnyDataValue>) -> Array {
    row.into_iter()
        .map(|v| match v.value_domain() {
            nemo_physical::datavalues::ValueDomain::PlainString => JsValue::from(v.lexical_value()),
            nemo_physical::datavalues::ValueDomain::LanguageTaggedString
            | nemo_physical::datavalues::ValueDomain::Null
            | nemo_physical::datavalues::ValueDomain::Other => JsValue::from(v.canonical_string()),
            nemo_physical::datavalues::ValueDomain::Iri => JsValue::from(v.to_iri_unchecked()),
            nemo_physical::datavalues::ValueDomain::Double => JsValue::from(v.to_f64_unchecked()),
            nemo_physical::datavalues::ValueDomain::Float => JsValue::from(v.to_f32_unchecked()),
            nemo_physical::datavalues::ValueDomain::UnsignedLong => {
                JsValue::from(v.to_u64_unchecked())
            }
            nemo_physical::datavalues::ValueDomain::NonNegativeLong
            | nemo_physical::datavalues::ValueDomain::UnsignedInt
            | nemo_physical::datavalues::ValueDomain::NonNegativeInt
            | nemo_physical::datavalues::ValueDomain::Long
            | nemo_physical::datavalues::ValueDomain::Int => JsValue::from(v.to_i64_unchecked()),
            nemo_physical::datavalues::ValueDomain::Boolean => {
                JsValue::from(v.to_boolean_unchecked())
            }
            nemo_physical::datavalues::ValueDomain::Tuple
            | nemo_physical::datavalues::ValueDomain::Map => {
                // Currently we only create a string representation of the tuple or map
                // We convert the tuples/maps to arrays/objects in the future if we require this in JavaScript
                JsValue::from(v.to_string())
            }
        })
        .collect()
}

#[wasm_bindgen(js_name = "setPanicHook")]
pub fn set_panic_hook() {
    console_error_panic_hook::set_once();
}

fn custom_alloc_error_hook(layout: Layout) {
    panic!(
        "memory allocation of {} bytes (align {}) failed",
        layout.size(),
        layout.align()
    );
}

#[wasm_bindgen(js_name = "setAllocErrorHook")]
pub fn set_alloc_error_hook() {
    std::alloc::set_alloc_error_hook(custom_alloc_error_hook);
}

#[wasm_bindgen(js_name = "getNemoVersion")]
pub fn nemo_version() -> Option<String> {
    option_env!("CARGO_PKG_VERSION").map(|version| version.into())
}
