//! Host support for nested virtualization.
//!
//! A sandbox with nested virtualization enabled sees hardware virtualization and can run its
//! own VMs through `/dev/kvm`. The host must be able to expose it:
//!
//! - Linux x86_64: the `kvm_intel` or `kvm_amd` module has `nested` enabled.
//! - macOS on Apple silicon: Hypervisor.framework supports EL2 guests (an M3 or newer chip and
//!   macOS 15 or newer).
//!
//! Other hosts do not support it.

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const KVM_NESTED_PARAMETERS: [&str; 2] = [
    "/sys/module/kvm_intel/parameters/nested",
    "/sys/module/kvm_amd/parameters/nested",
];

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const HYPERVISOR_FRAMEWORK: &std::ffi::CStr =
    c"/System/Library/Frameworks/Hypervisor.framework/Versions/A/Hypervisor";

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Checks whether this host can expose nested virtualization to a sandbox.
///
/// Returns why it cannot when it is unavailable.
pub fn host_support() -> Result<(), String> {
    host_support_impl()
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn host_support_impl() -> Result<(), String> {
    let values: Vec<String> = KVM_NESTED_PARAMETERS
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .collect();
    if values.is_empty() {
        return Err(
            "nested virtualization needs the kvm_intel or kvm_amd module, which is not loaded"
                .into(),
        );
    }
    if values.iter().any(|value| kvm_nested_enabled(value)) {
        Ok(())
    } else {
        Err("nested virtualization is disabled in the host's kvm_intel or kvm_amd module".into())
    }
}

/// Parses a KVM `nested` module parameter: `kvm_intel` reports `Y`/`N`, `kvm_amd` `1`/`0`.
#[cfg(any(test, all(target_os = "linux", target_arch = "x86_64")))]
fn kvm_nested_enabled(value: &str) -> bool {
    matches!(value.trim(), "Y" | "y" | "1")
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn host_support_impl() -> Result<(), String> {
    type GetEl2Supported = unsafe extern "C" fn(*mut bool) -> i32;

    // SAFETY: the path is a NUL-terminated system framework path.
    let framework = unsafe { libc::dlopen(HYPERVISOR_FRAMEWORK.as_ptr(), libc::RTLD_LAZY) };
    if framework.is_null() {
        return Err(
            "nested virtualization needs Hypervisor.framework, which is unavailable".into(),
        );
    }
    // SAFETY: `framework` is a live handle and the symbol name is NUL-terminated.
    let symbol = unsafe { libc::dlsym(framework, c"hv_vm_config_get_el2_supported".as_ptr()) };
    let result = if symbol.is_null() {
        Err("nested virtualization requires macOS 15 or newer".into())
    } else {
        // SAFETY: the symbol is `hv_return_t hv_vm_config_get_el2_supported(bool *)` in
        // Hypervisor.framework since macOS 15, and `hv_return_t` is a 32-bit `mach_error_t`.
        let get_el2_supported: GetEl2Supported = unsafe { std::mem::transmute(symbol) };
        let mut supported = false;
        // SAFETY: `supported` outlives the call.
        match unsafe { get_el2_supported(&mut supported) } {
            0 if supported => Ok(()),
            0 => Err("nested virtualization requires an Apple M3 or newer chip".into()),
            status => Err(format!(
                "nested virtualization support check failed with status {status:#x}"
            )),
        }
    };
    // SAFETY: `framework` came from dlopen and the function pointer is not used after this.
    unsafe { libc::dlclose(framework) };
    result
}

#[cfg(not(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64")
)))]
fn host_support_impl() -> Result<(), String> {
    Err("nested virtualization is not supported on this host".into())
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #[test]
    fn kvm_nested_parameter_accepts_both_module_formats() {
        for enabled in ["Y\n", "y", "1\n"] {
            assert!(super::kvm_nested_enabled(enabled), "{enabled:?}");
        }
        for disabled in ["N\n", "0\n", "", "2"] {
            assert!(!super::kvm_nested_enabled(disabled), "{disabled:?}");
        }
    }

    #[test]
    fn unavailable_hosts_explain_why_nesting_is_unavailable() {
        if let Err(reason) = super::host_support() {
            assert!(reason.starts_with("nested virtualization"), "{reason}");
        }
    }
}
