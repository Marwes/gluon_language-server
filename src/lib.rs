#![cfg_attr(feature = "serde_macros", feature(custom_derive, plugin))]
#![cfg_attr(feature = "serde_macros", plugin(serde_macros))]

extern crate clap;

extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;

#[macro_use]
extern crate futures;
extern crate futures_cpupool;
extern crate jsonrpc_core;
extern crate tokio_core;
extern crate tokio_io;

extern crate env_logger;
extern crate gluon;
extern crate gluon_completion as completion;
extern crate gluon_format;
#[macro_use]
extern crate log;
extern crate url;
extern crate url_serde;

extern crate bytes;

extern crate languageserver_types;

macro_rules! log_message {
    ($sender: expr, $($ts: tt)+) => {
        if log_enabled!(::log::LogLevel::Debug) {
            ::log_message($sender, format!( $($ts)+ ))
        } else {
            Box::new(Ok(()).into_future())
        }
    }
}

pub mod rpc;
mod async_io;

use jsonrpc_core::{IoHandler, Value};

use url::Url;

use gluon::base::ast::{Expr, SpannedExpr, Typed};
use gluon::base::error::Errors;
use gluon::base::fnv::FnvMap;
use gluon::base::metadata::Metadata;
use gluon::base::pos::{self, BytePos, Line, Span, Spanned};
use gluon::base::source;
use gluon::base::symbol::Symbol;
use gluon::base::types::{ArcType, BuiltinType, Type, TypeCache};
use gluon::import::{Import, Importer};
use gluon::vm::internal::Value as GluonValue;
use gluon::vm::thread::{Thread, ThreadInternal};
use gluon::vm::macros::Error as MacroError;
use gluon::compiler_pipeline::{MacroExpandable, MacroValue, TypecheckValue, Typecheckable};
use gluon::{filename_to_module, new_vm, Compiler, Error as GluonError, Result as GluonResult,
            RootedThread};

use completion::CompletionSymbol;

use std::collections::BTreeMap;
use std::env;
use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::io::{self, BufReader};
use std::path::PathBuf;
use std::str;
use std::sync::{Arc, Mutex, RwLock};

use languageserver_types::*;

use futures::{future, AsyncSink, Future, IntoFuture, Sink, Stream};
use futures::future::Either;
use futures::sync::oneshot;
use futures::sync::mpsc;

use futures_cpupool::CpuPool;

use tokio_core::reactor;

use tokio_io::codec::{Framed, FramedParts};

use bytes::BytesMut;

pub type BoxFuture<I, E> = Box<Future<Item = I, Error = E> + Send + 'static>;

use rpc::*;

fn log_message(sender: mpsc::Sender<String>, message: String) -> BoxFuture<(), ()> {
    debug!("{}", message);
    let r = format!(
        r#"{{"jsonrpc": "2.0", "method": "window/logMessage", "params": {} }}"#,
        serde_json::to_value(&LogMessageParams {
            typ: MessageType::Log,
            message: message,
        }).unwrap()
    );
    Box::new(sender.send(r).map(|_| ()).map_err(|_| ()))
}

fn expr_to_kind(expr: &SpannedExpr<Symbol>, typ: &ArcType) -> SymbolKind {
    match expr.value {
        // import! "std/prelude.glu" will replace itself with a symbol like `std.prelude
        Expr::Ident(ref id) if id.name.declared_name().contains('.') => SymbolKind::Module,
        _ => type_to_kind(typ),
    }
}

fn type_to_kind(typ: &ArcType) -> SymbolKind {
    match **typ {
        _ if typ.as_function().is_some() => SymbolKind::Function,
        Type::Ident(ref id) if id.declared_name() == "Bool" => SymbolKind::Boolean,
        Type::Alias(ref alias) if alias.name.declared_name() == "Bool" => SymbolKind::Boolean,
        Type::Builtin(builtin) => match builtin {
            BuiltinType::Char | BuiltinType::String => SymbolKind::String,
            BuiltinType::Byte | BuiltinType::Int | BuiltinType::Float => SymbolKind::Number,
            BuiltinType::Array => SymbolKind::Array,
            BuiltinType::Function => SymbolKind::Function,
        },
        _ => SymbolKind::Variable,
    }
}

fn completion_symbol_to_symbol_information(
    lines: &source::Lines,
    symbol: Spanned<CompletionSymbol, BytePos>,
    uri: Url,
) -> Result<SymbolInformation, ServerError<()>> {
    let (kind, name) = match symbol.value {
        CompletionSymbol::Type { ref name, .. } => (SymbolKind::Class, name),
        CompletionSymbol::Value {
            ref name,
            ref typ,
            ref expr,
        } => {
            let kind = expr_to_kind(expr, typ);
            (kind, name)
        }
    };
    Ok(SymbolInformation {
        kind,
        location: Location {
            uri,

            range: byte_span_to_range(lines, symbol.span)?,
        },
        name: name.declared_name().to_string(),
        container_name: None,
    })
}

struct Module {
    lines: source::Lines,
    expr: SpannedExpr<Symbol>,
    source_string: String,
    uri: Url,
}

#[derive(Clone)]
struct CheckImporter(Arc<Mutex<FnvMap<String, Module>>>);
impl CheckImporter {
    fn new() -> CheckImporter {
        CheckImporter(Arc::new(Mutex::new(FnvMap::default())))
    }
}
impl Importer for CheckImporter {
    fn import(
        &self,
        compiler: &mut Compiler,
        vm: &Thread,
        module_name: &str,
        input: &str,
        expr: SpannedExpr<Symbol>,
    ) -> Result<(), MacroError> {
        let macro_value = MacroValue { expr: expr };
        let TypecheckValue { expr, typ } = macro_value.typecheck(compiler, vm, module_name, input)?;

        let lines = source::Lines::new(input.as_bytes().iter().cloned());
        let (metadata, _) = gluon::check::metadata::metadata(&*vm.global_env().get_env(), &expr);
        self.0.lock().unwrap().insert(
            module_name.into(),
            self::Module {
                lines: lines,
                expr: expr,
                source_string: input.into(),
                uri: module_name_to_file_(module_name)?,
            },
        );
        // Insert a global to ensure the globals type can be looked up
        vm.global_env()
            .set_global(Symbol::from(module_name), typ, metadata, GluonValue::Int(0))?;
        Ok(())
    }
}

struct Initialize(RootedThread);
impl LanguageServerCommand<InitializeParams> for Initialize {
    type Output = InitializeResult;
    type Error = InitializeError;
    fn execute(
        &self,
        change: InitializeParams,
    ) -> BoxFuture<InitializeResult, ServerError<InitializeError>> {
        let import = self.0.get_macros().get("import").expect("Import macro");
        let import = import
            .downcast_ref::<Import<CheckImporter>>()
            .expect("Check importer");
        if let Some(ref path) = change.root_path {
            import.add_path(path);
        }
        Box::new(
            Ok(InitializeResult {
                capabilities: ServerCapabilities {
                    text_document_sync: Some(TextDocumentSyncKind::Incremental),
                    completion_provider: Some(CompletionOptions {
                        resolve_provider: Some(true),
                        trigger_characters: vec![".".into()],
                    }),
                    hover_provider: Some(true),
                    document_formatting_provider: Some(true),
                    document_highlight_provider: Some(true),
                    document_symbol_provider: Some(true),
                    workspace_symbol_provider: Some(true),
                    ..ServerCapabilities::default()
                },
            }).into_future(),
        )
    }

    fn invalid_params(&self) -> Option<Self::Error> {
        Some(InitializeError { retry: false })
    }
}

fn retrieve_expr<F, R>(thread: &Thread, text_document_uri: &Url, f: F) -> Result<R, ServerError<()>>
where
    F: FnOnce(&Module) -> Result<R, ServerError<()>>,
{
    let filename = strip_file_prefix_with_thread(thread, text_document_uri);
    let module = filename_to_module(&filename);
    let import = thread.get_macros().get("import").expect("Import macro");
    let import = import
        .downcast_ref::<Import<CheckImporter>>()
        .expect("Check importer");
    let importer = import.importer.0.lock().unwrap();
    let source_module = importer.get(&module).ok_or_else(|| {
        ServerError {
            message: format!(
                "Module `{}` is not defined\n{:?}",
                module,
                importer.keys().collect::<Vec<_>>()
            ),
            data: None,
        }
    })?;
    f(source_module)
}

fn retrieve_expr_with_pos<F, R>(
    thread: &Thread,
    text_document_uri: &Url,
    position: &Position,
    f: F,
) -> Result<R, ServerError<()>>
where
    F: FnOnce(&SpannedExpr<Symbol>, BytePos)
        -> Result<R, ServerError<()>>,
{
    retrieve_expr(thread, text_document_uri, |module| {
        let Module {
            ref expr,
            ref lines,
            ..
        } = *module;
        let byte_pos = position_to_byte_pos(lines, position)?;

        f(expr, byte_pos)
    })
}

#[derive(Serialize, Deserialize)]
pub struct CompletionData {
    #[serde(with = "url_serde")] pub text_document_uri: Url,
    pub position: Position,
}

struct Completion(RootedThread);
impl LanguageServerCommand<TextDocumentPositionParams> for Completion {
    type Output = Vec<CompletionItem>;
    type Error = ();
    fn execute(
        &self,
        change: TextDocumentPositionParams,
    ) -> BoxFuture<Vec<CompletionItem>, ServerError<()>> {
        let result = (move || -> Result<_, _> {
            let thread = &self.0;
            let suggestions = retrieve_expr_with_pos(
                &thread,
                &change.text_document.uri,
                &change.position,
                |expr, byte_pos| Ok(completion::suggest(&*thread.get_env(), expr, byte_pos)),
            )?;

            let mut items: Vec<_> = suggestions
                .into_iter()
                .map(|ident| {
                    // Remove the `:Line x, Row y suffix`
                    let name: &str = ident.name.as_ref();
                    let label = String::from(name.split(':').next().unwrap_or(ident.name.as_ref()));
                    CompletionItem {
                        label: label,
                        detail: Some(format!("{}", ident.typ)),
                        kind: Some(CompletionItemKind::Variable),
                        data: Some(
                            serde_json::to_value(CompletionData {
                                text_document_uri: change.text_document.uri.clone(),
                                position: change.position,
                            }).expect("CompletionData"),
                        ),
                        ..CompletionItem::default()
                    }
                })
                .collect();

            items.sort_by(|l, r| l.label.cmp(&r.label));

            Ok(items)
        })();
        Box::new(result.into_future())
    }

    fn invalid_params(&self) -> Option<Self::Error> {
        None
    }
}

struct HoverCommand(RootedThread);
impl LanguageServerCommand<TextDocumentPositionParams> for HoverCommand {
    type Output = Hover;
    type Error = ();
    fn execute(&self, change: TextDocumentPositionParams) -> BoxFuture<Hover, ServerError<()>> {
        Box::new(
            (|| -> Result<_, _> {
                let thread = &self.0;
                retrieve_expr(thread, &change.text_document.uri, |module| {
                    let expr = &module.expr;
                    let byte_pos = position_to_byte_pos(&module.lines, &change.position)?;

                    let env = thread.get_env();
                    let (_, metadata_map) = gluon::check::metadata::metadata(&*env, &expr);
                    let opt_metadata = completion::get_metadata(&metadata_map, expr, byte_pos);
                    let extract = (completion::TypeAt { env: &*env }, completion::SpanAt);
                    Ok(
                        completion::completion(extract, expr, byte_pos)
                            .map(|(typ, span)| {
                                let contents = match opt_metadata.and_then(|m| m.comment.as_ref()) {
                                    Some(comment) => format!("{}\n\n{}", typ, comment),
                                    None => format!("{}", typ),
                                };
                                Hover {
                                    contents: vec![MarkedString::String(contents)],
                                    range: byte_span_to_range(&module.lines, span).ok(),
                                }
                            })
                            .unwrap_or_else(|()| {
                                Hover {
                                    contents: vec![],
                                    range: None,
                                }
                            }),
                    )
                })
            })()
                .into_future(),
        )
    }

    fn invalid_params(&self) -> Option<Self::Error> {
        None
    }
}

fn location_to_position(loc: &pos::Location) -> Position {
    Position {
        line: loc.line.to_usize() as u64,
        character: loc.column.to_usize() as u64,
    }
}
fn span_to_range(span: &Span<pos::Location>) -> Range {
    Range {
        start: location_to_position(&span.start),
        end: location_to_position(&span.end),
    }
}

fn byte_pos_to_position(lines: &source::Lines, pos: BytePos) -> Result<Position, ServerError<()>> {
    Ok(location_to_position(&lines.location(pos).ok_or_else(|| {
        ServerError::from(&"Unable to translate index to location")
    })?))
}

fn byte_span_to_range(
    lines: &source::Lines,
    span: Span<BytePos>,
) -> Result<Range, ServerError<()>> {
    Ok(Range {
        start: byte_pos_to_position(lines, span.start)?,
        end: byte_pos_to_position(lines, span.end)?,
    })
}

fn position_to_byte_pos(
    lines: &source::Lines,
    position: &Position,
) -> Result<BytePos, ServerError<()>> {
    let line_pos = lines
        .line(Line::from(position.line as usize))
        .ok_or_else(|| {
            ServerError {
                message: format!(
                    "Position ({}, {}) is out of range",
                    position.line,
                    position.character
                ),
                data: None,
            }
        })?;
    Ok(line_pos + BytePos::from(position.character as usize))
}

fn range_to_byte_span(
    lines: &source::Lines,
    range: &Range,
) -> Result<Span<BytePos>, ServerError<()>> {
    Ok(Span::new(
        position_to_byte_pos(lines, &range.start)?,
        position_to_byte_pos(lines, &range.end)?,
    ))
}


fn apply_change(
    source: &mut String,
    lines: &source::Lines,
    change: TextDocumentContentChangeEvent,
) -> Result<(), ServerError<()>> {
    info!("Applying change: {:?}", change);
    let span = match (change.range, change.range_length) {
        (None, None) => Span::new(0.into(), source.len().into()),
        (Some(range), None) | (Some(range), Some(_)) => range_to_byte_span(lines, &range)?,
        (None, Some(_)) => return Err("Invalid change".into()),
    };
    source.drain(span.start.to_usize()..span.end.to_usize());
    source.insert_str(span.start.to_usize(), &change.text);
    Ok(())
}

fn module_name_to_file_(s: &str) -> Result<Url, Box<StdError + Send + Sync>> {
    let mut result = s.replace(".", "/");
    result.push_str(".glu");
    let path = fs::canonicalize(&*result).or_else(|err| match env::current_dir() {
        Ok(path) => Ok(path.join(result)),
        Err(_) => Err(err),
    })?;
    Ok(url::Url::from_file_path(path)
        .or_else(|_| url::Url::from_file_path(s))
        .map_err(|_| {
            format!("Unable to convert module name to a url: `{}`", s)
        })?)
}

// FIXME This may not be correct as this assumes the module can be located from the current working
// directory
fn module_name_to_file(s: &str) -> Url {
    module_name_to_file_(s).unwrap()
}


fn strip_file_prefix_with_thread(thread: &Thread, url: &Url) -> String {
    let import = thread.get_macros().get("import").expect("Import macro");
    let import = import
        .downcast_ref::<Import<CheckImporter>>()
        .expect("Check importer");
    let paths = import.paths.read().unwrap();
    strip_file_prefix(&paths, url).unwrap_or_else(|err| panic!("{}", err))
}

pub fn strip_file_prefix(
    paths: &[PathBuf],
    url: &Url,
) -> Result<String, Box<StdError + Send + Sync>> {
    use std::env;

    let path = url.to_file_path().map_err(|_| "Expected a file uri")?;
    let name = match fs::canonicalize(&*path) {
        Ok(name) => name,
        Err(_) => env::current_dir()?.join(&*path),
    };

    for path in paths {
        let canonicalized = fs::canonicalize(path).ok();

        let result = canonicalized
            .as_ref()
            .and_then(|path| name.strip_prefix(path).ok())
            .and_then(|path| path.to_str());
        if let Some(path) = result {
            return Ok(format!("{}", path));
        }
    }
    Ok(format!(
        "{}",
        name.strip_prefix(&env::current_dir()?)
            .unwrap_or_else(|_| &name)
            .display()
    ))
}

fn typecheck(thread: &Thread, uri_filename: &Url, fileinput: &str) -> GluonResult<()> {
    let filename = strip_file_prefix_with_thread(thread, uri_filename);
    let name = filename_to_module(&filename);
    debug!("Loading: `{}`", name);
    let mut errors = Errors::new();
    let mut compiler = Compiler::new();
    // The parser may find parse errors but still produce an expression
    // For that case still typecheck the expression but return the parse error afterwards
    let mut expr = match compiler.parse_partial_expr(&TypeCache::new(), &name, fileinput) {
        Ok(expr) => expr,
        Err((None, err)) => return Err(err.into()),
        Err((Some(expr), err)) => {
            errors.push(err.into());
            expr
        }
    };
    if let Err(err) = expr.expand_macro(&mut compiler, thread, &name) {
        errors.push(err);
    }

    let check_result = (MacroValue { expr: &mut expr })
        .typecheck(&mut compiler, thread, &name, fileinput)
        .map(|value| value.typ);
    let typ = match check_result {
        Ok(typ) => typ,
        Err(err) => {
            errors.push(err);
            expr.env_type_of(&*thread.global_env().get_env())
        }
    };
    let metadata = Metadata::default();
    thread
        .global_env()
        .set_global(Symbol::from(&name[..]), typ, metadata, GluonValue::Int(0))?;
    let import = thread.get_macros().get("import").expect("Import macro");
    let import = import
        .downcast_ref::<Import<CheckImporter>>()
        .expect("Check importer");
    let mut importer = import.importer.0.lock().unwrap();

    let lines = source::Lines::new(fileinput.as_bytes().iter().cloned());
    importer.insert(
        name.into(),
        self::Module {
            lines: lines,
            expr: expr,
            source_string: fileinput.into(),
            uri: uri_filename.clone(),
        },
    );
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.into())
    }
}


fn create_diagnostics(
    diagnostics: &mut BTreeMap<Url, Vec<Diagnostic>>,
    filename: &Url,
    err: GluonError,
) {
    fn into_diagnostic<T>(err: pos::Spanned<T, pos::Location>) -> Diagnostic
    where
        T: fmt::Display,
    {
        Diagnostic {
            message: format!("{}", err.value),
            severity: Some(DiagnosticSeverity::Error),
            range: span_to_range(&err.span),
            source: Some("gluon".to_string()),
            ..Diagnostic::default()
        }
    }

    match err {
        GluonError::Typecheck(err) => diagnostics
            .entry(module_name_to_file(&err.source_name))
            .or_insert(Vec::new())
            .extend(err.errors().into_iter().map(into_diagnostic)),
        GluonError::Parse(err) => diagnostics
            .entry(module_name_to_file(&err.source_name))
            .or_insert(Vec::new())
            .extend(err.errors().into_iter().map(into_diagnostic)),
        GluonError::Multiple(errors) => for err in errors {
            create_diagnostics(diagnostics, filename, err);
        },
        err => diagnostics
            .entry(filename.clone())
            .or_insert(Vec::new())
            .push(Diagnostic {
                message: format!("{}", err),
                severity: Some(DiagnosticSeverity::Error),
                ..Diagnostic::default()
            }),
    }
}

fn schedule_diagnostics(
    handle: &reactor::Handle,
    cpu_pool: &CpuPool,
    thread: RootedThread,
    filename: Url,
    fileinput: String,
) {
    handle.spawn(cpu_pool.spawn_fn(move || {
        run_diagnostics(&thread, &filename, &fileinput);
        Ok(())
    }))
}

fn run_diagnostics(thread: &Thread, filename: &Url, fileinput: &str) {
    info!("Running diagnostics on {}", filename);

    let diagnostics = match typecheck(thread, filename, fileinput) {
        Ok(_) => Some((filename.clone(), vec![])).into_iter().collect(),
        Err(err) => {
            debug!("Diagnostics result on `{}`: {}", filename, err);
            let mut diagnostics = BTreeMap::new();
            create_diagnostics(&mut diagnostics, filename, err);
            diagnostics
        }
    };
    for (source_name, diagnostic) in diagnostics {
        let r = format!(
            r#"{{
                            "jsonrpc": "2.0",
                            "method": "textDocument/publishDiagnostics",
                            "params": {}
                        }}"#,
            serde_json::to_value(&PublishDiagnosticsParams {
                uri: source_name,
                diagnostics: diagnostic,
            }).unwrap()
        );
        print!("Content-Length: {}\r\n\r\n{}", r.len(), r);
    }
}

pub fn run() {
    ::env_logger::init().unwrap();

    let _matches = clap::App::new("debugger")
        .version(env!("CARGO_PKG_VERSION"))
        .get_matches();

    let thread = new_vm();

    start_server(thread, io::stdin(), io::stdout()).unwrap();
}

pub fn start_server<R, W>(
    thread: RootedThread,
    input: R,
    mut output: W,
) -> Result<(), Box<StdError + Send + Sync>>
where
    R: io::Read + Send + 'static,
    W: io::Write + Send,
{
    {
        let macros = thread.get_macros();
        let mut check_import = Import::new(CheckImporter::new());
        {
            let import = macros.get("import").expect("Import macro");
            let import = import.downcast_ref::<Import>().expect("Importer");
            check_import.paths = RwLock::new((*import.paths.read().unwrap()).clone());
            check_import.loaders = RwLock::new(
                import
                    .loaders
                    .read()
                    .unwrap()
                    .iter()
                    .map(|(k, v)| (k.clone(), *v))
                    .collect(),
            );
        }
        macros.insert("import".into(), check_import);
    }

    let mut core = reactor::Core::new().unwrap();
    let (io, exit_receiver, _cpu_pool, message_log_receiver, message_log) =
        initialize_rpc(&thread, core.remote());

    let input = BufReader::new(async_io::async_read(input));

    let parts = FramedParts {
        inner: input,
        readbuf: BytesMut::default(),
        writebuf: BytesMut::default(),
    };
    let future: BoxFuture<(), Box<StdError + Send + Sync>> = Box::new(
        Framed::from_parts(parts, rpc::LanguageServerDecoder::new()).for_each(move |json| {
            debug!("Handle: {}", json);
            let message_log = message_log.clone();
            io.handle_request(&json).then(move |result| {
                if let Ok(Some(response)) = result {
                    Either::A(
                        message_log
                            .clone()
                            .send(response)
                            .map(|_| ())
                            .map_err(|_| "Unable to send".into()),
                    )
                } else {
                    Either::B(Ok(()).into_future())
                }
            })
        }).then(|x| {
            info!("{:?}", x);
            x
        }),
    );

    let log_messages = Box::new(
        message_log_receiver
            .map_err(|_| {
                let x: Box<StdError + Send + Sync> = "Unable to log message".into();
                x
            })
            .for_each(|message| -> Result<(), Box<StdError + Send + Sync>> {
                write!(
                    output,
                    "Content-Length: {}\r\n\r\n{}",
                    message.len(),
                    message
                )?;
                output.flush().unwrap();
                Ok(())
            }),
    );

    core.run(
        future::select_all(vec![
            future,
            Box::new(
                exit_receiver
                    .map(|_| {
                        info!("Exiting");
                    })
                    .map_err(|_| "Exit was canceled".into()),
            ),
            log_messages,
        ]).map(|t| t.0)
            .map_err(|t| t.0),
    )?;

    Ok(())
}

fn initialize_rpc(
    thread: &RootedThread,
    core_remote: reactor::Remote,
) -> (
    IoHandler,
    oneshot::Receiver<()>,
    CpuPool,
    mpsc::Receiver<String>,
    mpsc::Sender<String>,
) {
    let cpu_pool = futures_cpupool::Builder::new()
        .pool_size(2)
        .name_prefix("gluon_worker_")
        .create();

    let (message_log, message_log_receiver) = mpsc::channel(1);

    let work_queue = {
        let thread = thread.clone();
        let core_remote = core_remote.clone();
        let cpu_pool = cpu_pool.clone();
        SharedSink::new(rpc::UniqueQueue::new(
            rpc::sink_fn::<_, _, ()>(move |entry: Entry<Url, String>| {
                let thread = thread.clone();
                let cpu_pool = cpu_pool.clone();
                core_remote.spawn(move |core_handle| {
                    schedule_diagnostics(core_handle, &cpu_pool, thread, entry.key, entry.value);
                    Ok(())
                });
                Ok(AsyncSink::Ready)
            }),
        ))
    };

    let mut io = IoHandler::new();
    io.add_async_method(
        "initialize",
        ServerCommand::method(Initialize(thread.clone())),
    );
    io.add_async_method(
        "textDocument/completion",
        ServerCommand::method(Completion(thread.clone())),
    );

    {
        let thread = thread.clone();
        let message_log = message_log.clone();
        let resolve = move |mut item: CompletionItem| -> BoxFuture<CompletionItem, _> {
            let data: CompletionData =
                serde_json::from_value(item.data.clone().unwrap()).expect("CompletionData");

            let message_log2 = message_log.clone();
            let thread = thread.clone();
            let label = item.label.clone();
            Box::new(
                log_message!(message_log.clone(), "{:?}", data.text_document_uri)
                    .then(move |_| {
                        retrieve_expr_with_pos(
                            &thread,
                            &data.text_document_uri,
                            &data.position,
                            |expr, byte_pos| {
                                let type_env = thread.global_env().get_env();
                                let (_, metadata_map) =
                                    gluon::check::metadata::metadata(&*type_env, expr);
                                Ok(
                                    completion::suggest_metadata(
                                        &metadata_map,
                                        &*type_env,
                                        expr,
                                        byte_pos,
                                        &label,
                                    ).and_then(|metadata| metadata.comment.clone()),
                                )
                            },
                        )
                    })
                    .and_then(move |comment| {
                        log_message!(message_log2, "{:?}", comment)
                            .map(move |()| {
                                item.documentation = comment;
                                item
                            })
                            .map_err(|_| panic!("Unable to send log message"))
                    }),
            )
        };
        io.add_async_method("completionItem/resolve", ServerCommand::method(resolve));
    }

    io.add_async_method(
        "textDocument/hover",
        ServerCommand::method(HoverCommand(thread.clone())),
    );

    {
        let thread = thread.clone();
        let format = move |params: DocumentFormattingParams| -> BoxFuture<Vec<_>, _> {
            Box::new(
                retrieve_expr(&thread, &params.text_document.uri, |module| {
                    let formatted = gluon_format::format_expr(&module.source_string)?;
                    Ok(vec![
                        TextEdit {
                            range: byte_span_to_range(
                                &module.lines,
                                Span::new(0.into(), module.source_string.len().into()),
                            )?,
                            new_text: formatted,
                        },
                    ])
                }).into_future(),
            )
        };
        io.add_async_method("textDocument/formatting", ServerCommand::method(format));
    }

    {
        let thread = thread.clone();
        let f = move |params: TextDocumentPositionParams| -> BoxFuture<Vec<_>, _> {
            Box::new(
                retrieve_expr(&thread, &params.text_document.uri, |module| {
                    let expr = &module.expr;
                    let byte_pos = position_to_byte_pos(&module.lines, &params.position)?;

                    let symbol_spans = completion::find_all_symbols(expr, byte_pos)
                        .map(|t| t.1)
                        .unwrap_or(Vec::new());

                    symbol_spans
                        .into_iter()
                        .map(|span| {
                            Ok(DocumentHighlight {
                                kind: None,
                                range: byte_span_to_range(&module.lines, span)?,
                            })
                        })
                        .collect::<Result<_, _>>()
                }).into_future(),
            )
        };
        io.add_async_method("textDocument/documentHighlight", ServerCommand::method(f));
    }

    {
        let thread = thread.clone();
        let f = move |params: DocumentSymbolParams| -> BoxFuture<Vec<_>, _> {
            Box::new(
                retrieve_expr(&thread, &params.text_document.uri, |module| {
                    let expr = &module.expr;

                    let symbols = completion::all_symbols(expr);

                    symbols
                        .into_iter()
                        .map(|symbol| {
                            completion_symbol_to_symbol_information(
                                &module.lines,
                                symbol,
                                params.text_document.uri.clone(),
                            )
                        })
                        .collect::<Result<_, _>>()
                }).into_future(),
            )
        };
        io.add_async_method("textDocument/documentSymbol", ServerCommand::method(f));
    }

    {
        let cpu_pool = cpu_pool.clone();
        let thread = thread.clone();
        let f = move |params: WorkspaceSymbolParams| -> BoxFuture<Vec<_>, ServerError<()>> {
            let thread = thread.clone();
            Box::new(cpu_pool.spawn_fn(move || {
                let import = thread.get_macros().get("import").expect("Import macro");
                let import = import
                    .downcast_ref::<Import<CheckImporter>>()
                    .expect("Check importer");
                let modules = import.importer.0.lock().unwrap();

                let mut symbols = Vec::<SymbolInformation>::new();
                for module in modules.values() {
                    symbols.extend(completion::all_symbols(&module.expr)
                        .into_iter()
                        .filter(|symbol| match symbol.value {
                            CompletionSymbol::Value { ref name, .. } |
                            CompletionSymbol::Type { ref name, .. } => {
                                name.declared_name().contains(&params.query)
                            }
                        })
                        .map(|symbol| {
                            completion_symbol_to_symbol_information(
                                &module.lines,
                                symbol,
                                module.uri.clone(),
                            )
                        })
                        .collect::<Result<Vec<_>, _>>()?);
                }

                Ok(symbols)
            }))
        };
        io.add_async_method("workspace/symbol", ServerCommand::method(f));
    }

    io.add_async_method("shutdown", |_| Box::new(futures::finished(Value::from(0))));

    let (exit_sender, exit_receiver) = oneshot::channel();
    let exit_sender = Mutex::new(Some(exit_sender));
    io.add_notification("exit", move |_| {
        if let Some(exit_sender) = exit_sender.lock().unwrap().take() {
            exit_sender.send(()).unwrap()
        }
    });
    {
        let thread = thread.clone();
        let f = move |change: DidOpenTextDocumentParams| {
            // FIXME This should be scheduled on the cpu pool but we require that didOpen
            // finishes before any didChange commands so we run it synchronously for now
            run_diagnostics(
                &thread,
                &change.text_document.uri,
                &change.text_document.text,
            );
        };
        io.add_notification("textDocument/didOpen", ServerCommand::notification(f));
    }

    fn did_change<S>(
        thread: &Thread,
        sources: &Mutex<BTreeMap<Url, String>>,
        message_log: mpsc::Sender<String>,
        work_queue: S,
        change: DidChangeTextDocumentParams,
    ) -> BoxFuture<(), ()>
    where
        S: Sink<SinkItem = Entry<Url, String>, SinkError = ()> + Send + 'static,
    {
        let mut sources = sources.lock().unwrap();
        let source = sources
            .entry(change.text_document.uri.clone())
            .or_insert_with(|| {
                // If it does not exist in sources it should exist in the `import` macro
                let import = thread.get_macros().get("import").expect("Import macro");
                let import = import
                    .downcast_ref::<Import<CheckImporter>>()
                    .expect("Check importer");
                let modules = import.importer.0.lock().unwrap();
                let paths = import.paths.read().unwrap();
                let module_name = strip_file_prefix(&paths, &change.text_document.uri)
                    .unwrap_or_else(|err| panic!("{}", err));
                let module_name = filename_to_module(&module_name);
                modules
                    .get(&module_name)
                    .unwrap_or_else(|| {
                        panic!(
                            "textDocument/didChange processed before didOpen for module `{}`.\n \
                             Loaded modules: {:?}",
                            module_name,
                            modules.keys().collect::<Vec<_>>()
                        )
                    })
                    .source_string
                    .clone()
            });
        for change in change.content_changes {
            let lines = source::Lines::new(source.as_bytes().iter().cloned());
            match apply_change(source, &lines, change) {
                Ok(()) => (),
                Err(err) => return log_message!(message_log.clone(), "{}", err.message),
            }
        }
        debug!("Change source {}:\n{}", change.text_document.uri, source);

        Box::new(
            work_queue
                .send(Entry {
                    key: change.text_document.uri,
                    value: source.clone(),
                })
                .map(|_| ()),
        )
    }
    {
        let sources = Arc::new(Mutex::new(BTreeMap::new()));
        let thread = thread.clone();
        let message_log = message_log.clone();
        let f = move |change: DidChangeTextDocumentParams| {
            let work_queue = work_queue.clone();
            let sources = sources.clone();
            let thread = thread.clone();
            let message_log = message_log.clone();
            core_remote.spawn(move |_| {
                let work_queue = work_queue.clone();
                let sources = sources.clone();
                did_change(
                    &thread,
                    &sources,
                    message_log.clone(),
                    work_queue.clone(),
                    change,
                )
            });
        };

        io.add_notification("textDocument/didChange", ServerCommand::notification(f));
    }
    (
        io,
        exit_receiver,
        cpu_pool,
        message_log_receiver,
        message_log,
    )
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::path::PathBuf;

    use url::Url;

    use super::strip_file_prefix;

    #[test]
    fn test_strip_file_prefix() {
        let renamed = strip_file_prefix(
            &[PathBuf::from(".")],
            &Url::from_file_path(env::current_dir().unwrap().join("test")).unwrap(),
        ).unwrap();
        assert_eq!(renamed, "test");
    }
}
