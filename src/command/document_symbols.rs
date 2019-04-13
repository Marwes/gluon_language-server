use languageserver_types::{DocumentSymbolParams, DocumentSymbolResponse};

use completion;

use super::*;

pub fn register(io: &mut IoHandler, thread: &RootedThread) {
    let thread = thread.clone();
    let f = move |params: DocumentSymbolParams| {
        retrieve_expr(&thread, &params.text_document.uri, |module| {
            let expr = &module.expr;

            let symbols = completion::all_symbols(module.source.span(), expr);

            let source = &module.source;

            symbols
                .iter()
                .map(|symbol| completion_symbol_to_document_symbol(&source, symbol))
                .collect::<Result<_, _>>()
                .map(|x| Some(DocumentSymbolResponse::Nested(x)))
        })
    };
    io.add_async_method(request!("textDocument/documentSymbol"), f);
}