#![warn(
    // Harden built-in lints
    missing_copy_implementations,
    missing_debug_implementations,

    // Harden clippy lints
    clippy::cargo_common_metadata,
    clippy::clone_on_ref_ptr,
    clippy::dbg_macro,
    clippy::decimal_literal_representation,
    clippy::float_cmp_const,
    clippy::get_unwrap,
    clippy::integer_arithmetic,
    clippy::integer_division,
    clippy::pedantic,
)]
#![allow(
    // filter().map() can sometimes be more readable
    clippy::filter_map,
    // Most integer arithmetics are within an allocated region, so we know it's safe
    clippy::integer_arithmetic,
)]

mod builtins;
mod eval;
mod lookup;
mod parse;
mod scope;
mod tests;
mod utils;
mod value;

use dirs::home_dir;
use eval::Tree;
use gc::{Finalize, Gc, GcCell, Trace};
use log::{error, trace, warn};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use lsp_types::{
    notification::{Notification as _, *},
    request::{Request as RequestTrait, *},
    *,
};
use rnix::{
    parser::*,
    types::*,
    value::{Anchor as RAnchor, Value as RValue},
    SyntaxNode, TextRange, TextSize,
};
use scope::Scope;
use std::{
    collections::HashMap,
    panic,
    path::{Path, PathBuf},
    process,
    rc::Rc,
    str::FromStr,
};
use value::NixValue;

type Error = Box<dyn std::error::Error>;
#[derive(Debug, Clone, Trace, Finalize)]
pub enum EvalError {
    Unimplemented(String),
    Unexpected(String),
    StackTrace(String),
    TypeError(String),
    Parsing,
    Unknown,
}

impl std::error::Error for EvalError {}

impl std::fmt::Display for EvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            EvalError::Unimplemented(msg) => write!(f, "unimplemented: {}", msg),
            EvalError::Unexpected(msg) => write!(f, "unexpected: {}", msg),
            EvalError::StackTrace(msg) => write!(f, "{}", msg),
            EvalError::TypeError(msg) => write!(f, "type error: {}", msg),
            EvalError::Parsing => write!(f, "parsing error"),
            EvalError::Unknown => write!(f, "unknown value"),
        }
    }
}

impl From<&EvalError> for EvalError {
    fn from(x: &EvalError) -> Self {
        x.clone()
    }
}

fn main() {
    if let Err(err) = real_main() {
        error!("Error: {} ({:?})", err, err);
        error!("A fatal error has occured and rnix-lsp will shut down.");
        drop(err);
        process::exit(libc::EXIT_FAILURE);
    }
}
fn real_main() -> Result<(), Error> {
    env_logger::init();
    panic::set_hook(Box::new(move |panic| {
        error!("----- Panic -----");
        error!("{}", panic);
    }));

    let (connection, io_threads) = Connection::stdio();
    let capabilities = serde_json::to_value(&ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::Full),
                ..TextDocumentSyncOptions::default()
            },
        )),
        completion_provider: Some(CompletionOptions {
            ..CompletionOptions::default()
        }),
        definition_provider: Some(true),
        document_formatting_provider: Some(true),
        document_link_provider: Some(DocumentLinkOptions {
            resolve_provider: Some(false),
            work_done_progress_options: WorkDoneProgressOptions::default(),
        }),
        hover_provider: Some(true),
        rename_provider: Some(RenameProviderCapability::Simple(true)),
        selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
        ..ServerCapabilities::default()
    })
    .unwrap();

    connection.initialize(capabilities)?;

    App {
        files: HashMap::new(),
        store: Gc::new(GcCell::new(HashMap::new())),
        conn: connection,
    }
    .main();

    io_threads.join()?;

    Ok(())
}

struct App {
    files: HashMap<Url, (AST, String, Result<Tree, EvalError>)>,
    store: Gc<GcCell<HashMap<String, Gc<NixValue>>>>,
    conn: Connection,
}
impl App {
    fn reply(&mut self, response: Response) {
        trace!("Sending response: {:#?}", response);
        self.conn.sender.send(Message::Response(response)).unwrap();
    }
    fn notify(&mut self, notification: Notification) {
        trace!("Sending notification: {:#?}", notification);
        self.conn
            .sender
            .send(Message::Notification(notification))
            .unwrap();
    }
    fn err<E>(&mut self, id: RequestId, err: E)
    where
        E: std::fmt::Display,
    {
        warn!("{}", err);
        self.reply(Response::new_err(
            id,
            ErrorCode::UnknownErrorCode as i32,
            err.to_string(),
        ));
    }
    fn main(&mut self) {
        while let Ok(msg) = self.conn.receiver.recv() {
            trace!("Message: {:#?}", msg);
            match msg {
                Message::Request(req) => {
                    let id = req.id.clone();
                    match self.conn.handle_shutdown(&req) {
                        Ok(true) => break,
                        Ok(false) => self.handle_request(req),
                        Err(err) => {
                            // This only fails if a shutdown was
                            // requested in the first place, so it
                            // should definitely break out of the
                            // loop.
                            self.err(id, err);
                            break;
                        }
                    }
                }
                Message::Notification(notification) => {
                    let _ = self.handle_notification(notification);
                }
                Message::Response(_) => (),
            }
        }
    }
    fn handle_request(&mut self, req: Request) {
        fn cast<Kind>(req: &mut Option<Request>) -> Option<(RequestId, Kind::Params)>
        where
            Kind: RequestTrait,
            Kind::Params: serde::de::DeserializeOwned,
        {
            match req.take().unwrap().extract::<Kind::Params>(Kind::METHOD) {
                Ok(value) => Some(value),
                Err(owned) => {
                    *req = Some(owned);
                    None
                }
            }
        }
        let mut req = Some(req);
        if let Some((id, params)) = cast::<GotoDefinition>(&mut req) {
            if let Some(pos) = self.lookup_definition(params) {
                self.reply(Response::new_ok(id, pos));
            } else {
                self.reply(Response::new_ok(id, ()));
            }
        } else if let Some((id, params)) = cast::<Completion>(&mut req) {
            let completions = self
                .completions(&params.text_document_position)
                .unwrap_or_default();
            self.reply(Response::new_ok(id, completions));
        } else if let Some((id, params)) = cast::<Rename>(&mut req) {
            let changes = self.rename(params);
            self.reply(Response::new_ok(
                id,
                WorkspaceEdit {
                    changes,
                    ..WorkspaceEdit::default()
                },
            ));
        } else if let Some((id, params)) = cast::<DocumentLinkRequest>(&mut req) {
            let document_links = self.document_links(&params).unwrap_or_default();
            self.reply(Response::new_ok(id, document_links));
        } else if let Some((id, params)) = cast::<Formatting>(&mut req) {
            let changes = if let Some((ast, code, _)) = self.files.get(&params.text_document.uri) {
                let fmt = nixpkgs_fmt::reformat_node(&ast.node());
                vec![TextEdit {
                    range: utils::range(&code, TextRange::up_to(ast.node().text().len())),
                    new_text: fmt.text().to_string(),
                }]
            } else {
                Vec::new()
            };
            self.reply(Response::new_ok(id, changes));
        } else if let Some((id, params)) = cast::<HoverRequest>(&mut req) {
            if let Some((range, markdown)) = self.handle_hover(params) {
                self.reply(Response::new_ok(
                    id,
                    Hover {
                        contents: HoverContents::Markup(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: markdown,
                        }),
                        range,
                    },
                ));
            } else {
                self.reply(Response::new_ok(id, ()));
            }
        } else if let Some((id, params)) = cast::<SelectionRangeRequest>(&mut req) {
            let mut selections = Vec::new();
            if let Some((ast, code, _)) = self.files.get(&params.text_document.uri) {
                for pos in params.positions {
                    selections.push(utils::selection_ranges(&ast.node(), code, pos));
                }
            }
            self.reply(Response::new_ok(id, selections));
        } else {
            let req = req.expect("internal error: req should have been wrapped in Some");

            self.reply(Response::new_err(
                req.id,
                ErrorCode::MethodNotFound as i32,
                format!("Unhandled method {}", req.method),
            ))
        }
    }
    fn handle_notification(&mut self, req: Notification) -> Result<(), Error> {
        match &*req.method {
            DidOpenTextDocument::METHOD => {
                let params: DidOpenTextDocumentParams = serde_json::from_value(req.params)?;
                let text = params.text_document.text;
                let parsed = rnix::parse(&text);
                self.send_diagnostics(params.text_document.uri.clone(), &text, &parsed)?;
                if let Ok(path) = PathBuf::from_str(params.text_document.uri.path()) {
                    let gc_root = Gc::new(Scope::Root(path, Some(self.store.clone())));
                    let parsed_root = parsed.root().inner().ok_or(EvalError::Parsing);
                    let evaluated = parsed_root.and_then(|x| Tree::parse_legacy(&x, gc_root));
                    self.files
                        .insert(params.text_document.uri, (parsed, text, evaluated));
                }
            }
            DidChangeTextDocument::METHOD => {
                let params: DidChangeTextDocumentParams = serde_json::from_value(req.params)?;
                if let Some(change) = params.content_changes.into_iter().last() {
                    let parsed = rnix::parse(&change.text);
                    self.send_diagnostics(params.text_document.uri.clone(), &change.text, &parsed)?;
                    if let Ok(path) = PathBuf::from_str(params.text_document.uri.path()) {
                        let gc_root = Gc::new(Scope::Root(path, Some(self.store.clone())));
                        let parsed_root = parsed.root().inner().ok_or(EvalError::Parsing);
                        let evaluated = parsed_root.and_then(|x| Tree::parse_legacy(&x, gc_root));
                        self.files
                            .insert(params.text_document.uri, (parsed, change.text, evaluated));
                    }
                }
            }
            _ => (),
        }
        Ok(())
    }
    fn lookup_definition(&mut self, params: TextDocumentPositionParams) -> Option<Location> {
        let (current_ast, current_content, _) = self.files.get(&params.text_document.uri)?;
        let offset = utils::lookup_pos(current_content, params.position)?;
        let node = current_ast.node();
        let (name, scope, _) = self.scope_for_ident(params.text_document.uri, &node, offset)?;

        let var_e = scope.get(name.as_str())?;
        if let Some(var) = &var_e.var {
            let (_definition_ast, definition_content, _) = self.files.get(&var.file)?;
            Some(Location {
                uri: (*var.file).clone(),
                range: utils::range(definition_content, var.key.text_range()),
            })
        } else {
            None
        }
    }
    fn handle_hover(&self, params: TextDocumentPositionParams) -> Option<(Option<Range>, String)> {
        let (_, content, tree) = self.files.get(&params.text_document.uri)?;
        let offset = utils::lookup_pos(content, params.position)?;
        let tree = match tree {
            Ok(x) => {
                let tmp = Gc::new(x.clone());
                climb_tree(&tmp, offset).clone()
            }
            Err(e) => return Some((None, format!("{:?}", e))),
        };
        let range = utils::range(content, tree.range?);
        let val = match tree.eval() {
            Ok(x) => x.format_markdown(),
            Err(e) => format!("{}", e),
        };
        Some((Some(range), val))
    }
    #[allow(clippy::shadow_unrelated)] // false positive
    fn completions(&mut self, params: &TextDocumentPositionParams) -> Option<Vec<CompletionItem>> {
        let (ast, content, _) = self.files.get(&params.text_document.uri)?;
        let offset = utils::lookup_pos(content, params.position)?;

        let node = ast.node();
        let (node, scope, name) =
            self.scope_for_ident(params.text_document.uri.clone(), &node, offset)?;

        // Re-open, because scope_for_ident may mutably borrow
        let (_, content, _) = self.files.get(&params.text_document.uri)?;

        let mut completions = Vec::new();
        for (var, data) in scope {
            if var.starts_with(&name.as_str()) {
                let det = data.render_detail();
                completions.push(CompletionItem {
                    label: var.clone(),
                    documentation: data
                        .documentation
                        .map(|x| lsp_types::Documentation::String(x)),
                    deprecated: Some(data.deprecated),
                    text_edit: Some(TextEdit {
                        range: utils::range(content, node.node().text_range()),
                        new_text: var.clone(),
                    }),
                    detail: Some(det),
                    ..CompletionItem::default()
                });
            }
        }
        Some(completions)
    }
    fn rename(&mut self, params: RenameParams) -> Option<HashMap<Url, Vec<TextEdit>>> {
        struct Rename<'a> {
            edits: Vec<TextEdit>,
            code: &'a str,
            old: &'a str,
            new_name: String,
        }
        fn rename_in_node(rename: &mut Rename, node: &SyntaxNode) -> Option<()> {
            if let Some(ident) = Ident::cast(node.clone()) {
                if ident.as_str() == rename.old {
                    rename.edits.push(TextEdit {
                        range: utils::range(rename.code, node.text_range()),
                        new_text: rename.new_name.clone(),
                    });
                }
            } else if let Some(index) = Select::cast(node.clone()) {
                rename_in_node(rename, &index.set()?);
            } else if let Some(attr) = Key::cast(node.clone()) {
                let mut path = attr.path();
                if let Some(ident) = path.next() {
                    rename_in_node(rename, &ident);
                }
            } else {
                for child in node.children() {
                    rename_in_node(rename, &child);
                }
            }
            Some(())
        }

        let uri = params.text_document_position.text_document.uri;
        let (ast, code, _) = self.files.get(&uri)?;
        let offset = utils::lookup_pos(code, params.text_document_position.position)?;
        let info = utils::ident_at(&ast.node(), offset)?;
        if !info.path.is_empty() {
            // Renaming within a set not supported
            return None;
        }
        let old = info.ident;
        let scope = utils::scope_for(&Rc::new(uri.clone()), old.node().clone())?;

        let mut rename = Rename {
            edits: Vec::new(),
            code,
            old: old.as_str(),
            new_name: params.new_name,
        };
        let definition = scope.get(old.as_str())?;
        rename_in_node(&mut rename, &definition.set);

        let mut changes = HashMap::new();
        changes.insert(uri, rename.edits);
        Some(changes)
    }
    fn document_links(&mut self, params: &DocumentLinkParams) -> Option<Vec<DocumentLink>> {
        let (current_ast, current_content, _) = self.files.get(&params.text_document.uri)?;
        let parent_dir = Path::new(params.text_document.uri.path()).parent();
        let home_dir = home_dir();
        let home_dir = home_dir.as_ref();
        let mut document_links = vec![];
        for node in current_ast.node().descendants() {
            let value = Value::cast(node.clone()).and_then(|v| v.to_value().ok());
            if let Some(RValue::Path(anchor, path)) = value {
                let file_url = match anchor {
                    RAnchor::Absolute => Some(PathBuf::from(&path)),
                    RAnchor::Relative => parent_dir.map(|p| p.join(path)),
                    RAnchor::Home => home_dir.map(|home| home.join(path)),
                    RAnchor::Store => None,
                }
                .and_then(|path| std::fs::canonicalize(&path).ok())
                .filter(|path| path.is_file())
                .and_then(|s| Url::parse(&format!("file://{}", s.to_string_lossy())).ok());
                if let Some(file_url) = file_url {
                    document_links.push(DocumentLink {
                        target: file_url,
                        range: utils::range(current_content, node.text_range()),
                        tooltip: None,
                    })
                }
            }
        }
        Some(document_links)
    }
    fn send_diagnostics(&mut self, uri: Url, code: &str, ast: &AST) -> Result<(), Error> {
        let errors = ast.errors();
        let mut diagnostics = Vec::with_capacity(errors.len());
        for err in errors {
            let node_range = match err {
                ParseError::Unexpected(range)
                | ParseError::UnexpectedDoubleBind(range)
                | ParseError::UnexpectedExtra(range)
                | ParseError::UnexpectedWanted(_, range, _) => Some(range),
                ParseError::UnexpectedEOF | ParseError::UnexpectedEOFWanted(_) => {
                    Some(TextRange::at(TextSize::of(code), TextSize::from(0)))
                }
                _ => None,
            };
            if let Some(node_range) = node_range {
                diagnostics.push(Diagnostic {
                    range: utils::range(code, node_range),
                    severity: Some(DiagnosticSeverity::Error),
                    message: err.to_string(),
                    ..Diagnostic::default()
                });
            }
        }
        self.notify(Notification::new(
            "textDocument/publishDiagnostics".into(),
            PublishDiagnosticsParams {
                uri,
                diagnostics,
                version: None,
            },
        ));
        Ok(())
    }
}

fn climb_tree(here: &Gc<Tree>, offset: usize) -> &Gc<Tree> {
    for child in here.children().clone() {
        let range = match child.range {
            Some(x) => x,
            None => continue,
        };
        let start: usize = range.start().into();
        let end: usize = range.end().into();
        if start <= offset && offset <= end {
            return climb_tree(child, offset);
        }
    }
    here
}
