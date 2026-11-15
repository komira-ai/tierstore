//! The names the crate exports are the ones of the proto: the package, the service and its
//! methods, and the path of a call on the wire.

use tierstore_trees_api::{METHODS, PACKAGE, SERVICE, path};

/// Asserts the consts are what `trees.proto` declares, so that a path built from them is the path
/// the generated stubs serve and call.
#[test]
fn names_are_the_ones_of_the_proto() {
    let set =
        protox::compile(["tierstore/trees/v1/trees.proto"], ["proto"]).expect("the proto compiles");
    let file = &set.file[0];
    assert_eq!(file.package(), PACKAGE);
    assert_eq!(file.service.len(), 1);
    let service = &file.service[0];
    assert_eq!(service.name(), SERVICE);
    let methods: Vec<&str> = service.method.iter().map(|m| m.name()).collect();
    assert_eq!(methods, METHODS);
}

/// Asserts the path of a call is `/<package>.<service>/<method>`, the form a signature covers.
#[test]
fn path_of_a_call_is_package_service_method() {
    assert_eq!(
        path("CreateTree"),
        "/tierstore.trees.v1.TreesService/CreateTree"
    );
    assert_eq!(
        path("GetPlacement"),
        "/tierstore.trees.v1.TreesService/GetPlacement"
    );
}
