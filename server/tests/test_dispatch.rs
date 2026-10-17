use lsp_server::{ErrorCode, Request, RequestId};
use lsp_types::request::{
    CallHierarchyIncomingCalls, CallHierarchyOutgoingCalls, CallHierarchyPrepare, References, Request as _,
    TypeHierarchyPrepare, TypeHierarchySubtypes, TypeHierarchySupertypes,
};
use odoo_ls_server::server::{is_main_thread_request, method_not_found_response};

#[test]
fn test_unknown_request_gets_method_not_found() {
    let request = Request {
        id: RequestId::from(7),
        method: "textDocument/someUnknownMethod".to_string(),
        params: serde_json::Value::Null,
    };
    assert!(!is_main_thread_request(&request.method));
    let response = method_not_found_response(&request);
    assert_eq!(response.id, RequestId::from(7));
    assert!(response.result.is_none());
    let error = response.error.expect("an error response");
    assert_eq!(error.code, ErrorCode::MethodNotFound as i32);
    assert!(error.message.contains("textDocument/someUnknownMethod"));
}

#[test]
fn test_known_request_goes_to_main_thread() {
    assert!(is_main_thread_request(References::METHOD));
    for method in [
        CallHierarchyPrepare::METHOD, CallHierarchyIncomingCalls::METHOD, CallHierarchyOutgoingCalls::METHOD,
        TypeHierarchyPrepare::METHOD, TypeHierarchySupertypes::METHOD, TypeHierarchySubtypes::METHOD,
    ] {
        assert!(is_main_thread_request(method), "{method} is routed to the main thread");
    }
}
