#[test]
fn public_patch_api_is_available_to_platform_adapters() {
    use yaat_lib::activation::{ConfigFormat, OwnedPath, PatchEngine, PatchOperation};

    let _engine = PatchEngine;
    let _format = ConfigFormat::Jsonc;
    let owned = OwnedPath::from_json_pointer("/env/API_KEY").unwrap();
    let operation = PatchOperation::set(owned, "<secret>");
    assert_eq!(operation.path().to_json_pointer(), "/env/API_KEY");
}
