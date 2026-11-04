//! A backend call run detached, through `ops::detached`.

use crate::Error;

use super::ops::detached;

fn goes_wrong() -> Result<(), Error> {
    panic!("the call went wrong");
}

/// Asserts that a call run detached gives back what the call returned, a value or its own error,
/// and that a call whose task panics comes back as a backend error that says the call did not
/// finish, rather than the panic carrying on into the request.
#[tokio::test]
async fn a_detached_call_gives_its_result_and_a_panic_is_a_backend_error() {
    assert_eq!(detached(async { Ok::<u8, Error>(7) }).await, Ok(7));
    let refused = Error::NotFound("k".to_owned());
    let failing = refused.clone();
    assert_eq!(
        detached(async move { Err::<u8, Error>(failing) }).await,
        Err(refused)
    );
    let Err(Error::Other(text)) = detached(async { goes_wrong() }).await else {
        panic!("a panicking call is a backend error");
    };
    assert!(
        text.starts_with("a call to the backend did not finish: task "),
        "{text}"
    );
    assert!(text.contains("panicked"), "{text}");
}
