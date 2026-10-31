//! Integration tests for changing nested virtualization on an existing sandbox.
//!
//! These tests require KVM (or libkrun on macOS). The `#[msb_test]`
//! attribute marks them `#[ignore]`, so plain `cargo test --workspace`
//! skips them. Run them via:
//!
//!     cargo nextest run -p microsandbox --test nested_virt_modify --run-ignored=only
//!
//! Set `MSB_TEST_EXPECT_NESTED_VIRT=1` where the host has nested KVM, as CI does, so the test
//! fails instead of checking only the refusal if the host check wrongly refuses it.

use microsandbox::{MicrosandboxError, Sandbox};
use test_utils::msb_test;

const IMAGE: &str = "mirror.gcr.io/library/alpine";

async fn cleanup(name: &str) {
    if let Ok(handle) = Sandbox::get(name).await {
        let _ = handle.kill().await;
        let _ = handle.remove().await;
    }
}

async fn has_kvm(sandbox: &Sandbox) -> bool {
    let output = sandbox
        .shell("test -c /dev/kvm")
        .await
        .expect("check for /dev/kvm");
    output.status().success
}

async fn running(name: &str) -> Sandbox {
    Sandbox::get(name)
        .await
        .expect("sandbox record")
        .connect()
        .await
        .expect("running sandbox")
}

/// Fails when the host is expected to support nesting but the host check refuses it.
async fn host_support() -> Result<(), String> {
    let backend = microsandbox::backend::default_backend();
    let local = backend.as_local().expect("a local backend");
    let support = microsandbox::runtime::nested_virt_host_support(local.config())
        .await
        .map_err(|error| error.to_string());
    if let Err(reason) = &support
        && std::env::var_os("MSB_TEST_EXPECT_NESTED_VIRT").is_some()
    {
        panic!("the host has nested KVM, but the host check refused it: {reason}");
    }
    support
}

/// `modify()` turns nesting on and off for an existing sandbox at the next boot. Hosts that
/// cannot expose it refuse the change before the sandbox is stopped.
#[msb_test]
async fn nested_virt_can_be_modified_on_an_existing_sandbox() {
    let name = "nested-virt-modify";
    cleanup(name).await;

    let sandbox = Sandbox::builder(name)
        .image(IMAGE)
        .cpus(1)
        .memory(512)
        .replace()
        .create()
        .await
        .expect("create default sandbox");
    assert!(!has_kvm(&sandbox).await);

    let enabled = sandbox.modify().nested_virt(true).restart().apply().await;
    if let Err(reason) = host_support().await {
        let error = enabled.expect_err("a host without nested virtualization must refuse it");
        assert!(
            matches!(error, MicrosandboxError::Unsupported { .. }),
            "{error}"
        );
        let sandbox = running(name).await;
        assert!(
            !has_kvm(&sandbox).await,
            "the sandbox must keep running unchanged"
        );
        // Make the reduced coverage visible instead of passing silently.
        eprintln!(
            "nested virtualization is unavailable on this host, checked the refusal only: {reason}"
        );
        sandbox.stop().await.ok();
        cleanup(name).await;
        return;
    }
    assert!(enabled.expect("enable nested virtualization").applied);
    assert!(
        has_kvm(&running(name).await).await,
        "enabling nested virtualization must expose /dev/kvm after the restart"
    );

    let disabled = running(name)
        .await
        .modify()
        .nested_virt(false)
        .restart()
        .apply()
        .await
        .expect("disable nested virtualization");
    assert!(disabled.applied);
    let sandbox = running(name).await;
    assert!(
        !has_kvm(&sandbox).await,
        "disabling nested virtualization must hide /dev/kvm after the restart"
    );

    sandbox.stop().await.ok();
    cleanup(name).await;
}
