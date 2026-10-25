//! Host support for nested virtualization.
//!
//! A sandbox with nested virtualization enabled sees hardware virtualization and can run its
//! own VMs through `/dev/kvm`. The host must be able to expose it:
//!
//! - Linux x86_64: the `kvm_intel` or `kvm_amd` module has `nested` enabled.
//! - macOS on Apple silicon: Hypervisor.framework supports EL2 guests (an M3 or newer chip and
//!   macOS 15 or newer).
//! - Windows x86_64: the Windows Hypervisor Platform sets up a partition with nested
//!   virtualization enabled (Windows 11 or Windows Server 2022 or newer).
//!
//! Other hosts do not support it.

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
use std::ffi::c_void;

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
use windows_sys::Win32::Foundation::FreeLibrary;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
use windows_sys::Win32::System::Hypervisor::{
    WHV_PARTITION_HANDLE, WHV_PARTITION_PROPERTY_CODE,
    WHvPartitionPropertyCodeLocalApicEmulationMode, WHvPartitionPropertyCodeNestedVirtualization,
    WHvPartitionPropertyCodeProcessorCount, WHvX64LocalApicEmulationModeXApic,
};
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
use windows_sys::core::HRESULT;

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
// Types
//--------------------------------------------------------------------------------------------------

/// An export as `GetProcAddress` returns it, before it is cast to its declared signature.
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
type WhpExport = unsafe extern "system" fn() -> isize;

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
type WhpCreatePartition = unsafe extern "system" fn(*mut WHV_PARTITION_HANDLE) -> HRESULT;

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
type WhpSetPartitionProperty = unsafe extern "system" fn(
    WHV_PARTITION_HANDLE,
    WHV_PARTITION_PROPERTY_CODE,
    *const c_void,
    u32,
) -> HRESULT;

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
type WhpPartitionCall = unsafe extern "system" fn(WHV_PARTITION_HANDLE) -> HRESULT;

/// The `WinHvPlatform.dll` exports that the Windows probe calls.
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
struct WhpPartitionApi {
    create: WhpCreatePartition,
    set_property: WhpSetPartitionProperty,
    setup: WhpPartitionCall,
    delete: WhpPartitionCall,
}

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

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn host_support_impl() -> Result<(), String> {
    // Load WHP at run time, as macOS loads Hypervisor.framework: importing it would keep every
    // process that links this crate from starting on hosts without the optional feature.
    // SAFETY: `w!` yields a NUL-terminated UTF-16 string.
    let module = unsafe {
        LoadLibraryExW(
            windows_sys::w!("WinHvPlatform.dll"),
            std::ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    };
    if module.is_null() {
        return Err(
            "nested virtualization needs the Windows Hypervisor Platform, which is not enabled"
                .into(),
        );
    }

    // SAFETY: `module` is a live handle and the symbol names are NUL-terminated.
    let symbols = unsafe {
        (
            GetProcAddress(module, c"WHvCreatePartition".as_ptr().cast()),
            GetProcAddress(module, c"WHvSetPartitionProperty".as_ptr().cast()),
            GetProcAddress(module, c"WHvSetupPartition".as_ptr().cast()),
            GetProcAddress(module, c"WHvDeletePartition".as_ptr().cast()),
        )
    };
    let result = match symbols {
        (Some(create), Some(set_property), Some(setup), Some(delete)) => {
            // SAFETY: each export has the signature that WinHvPlatform.h declares for it.
            let api = unsafe {
                WhpPartitionApi {
                    create: std::mem::transmute::<WhpExport, WhpCreatePartition>(create),
                    set_property: std::mem::transmute::<WhpExport, WhpSetPartitionProperty>(
                        set_property,
                    ),
                    setup: std::mem::transmute::<WhpExport, WhpPartitionCall>(setup),
                    delete: std::mem::transmute::<WhpExport, WhpPartitionCall>(delete),
                }
            };
            probe_whp_nested_partition(&api)
        }
        _ => Err("nested virtualization needs a newer Windows Hypervisor Platform".into()),
    };

    // SAFETY: `module` came from LoadLibraryExW and its functions are not used after this.
    unsafe { FreeLibrary(module) };
    result
}

#[cfg(not(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "windows", target_arch = "x86_64")
)))]
fn host_support_impl() -> Result<(), String> {
    Err("nested virtualization is not supported on this host".into())
}

//--------------------------------------------------------------------------------------------------
// Functions: Helpers
//--------------------------------------------------------------------------------------------------

/// Creates a one-vCPU partition, sets it up for nesting as the VMM does, and deletes it.
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn probe_whp_nested_partition(api: &WhpPartitionApi) -> Result<(), String> {
    let mut partition: WHV_PARTITION_HANDLE = 0;
    // SAFETY: `partition` outlives the call.
    let status = unsafe { (api.create)(&mut partition) };
    if status < 0 {
        return Err(format!(
            "nested virtualization support check failed to create a partition with status {status:#x}"
        ));
    }

    let result = set_up_nested_partition(api, partition);

    // SAFETY: `partition` is live and not used after this.
    unsafe { (api.delete)(partition) };
    result
}

/// Configures a new partition for nesting and sets it up, which is where WHP refuses it.
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn set_up_nested_partition(
    api: &WhpPartitionApi,
    partition: WHV_PARTITION_HANDLE,
) -> Result<(), String> {
    let set = |code: WHV_PARTITION_PROPERTY_CODE, value: u32| {
        // SAFETY: `partition` is live and `value` outlives the call.
        unsafe {
            (api.set_property)(
                partition,
                code,
                (&value as *const u32).cast(),
                size_of::<u32>() as u32,
            )
        }
    };

    let status = set(WHvPartitionPropertyCodeProcessorCount, 1);
    if status < 0 {
        return Err(format!(
            "nested virtualization support check failed to configure a partition with status {status:#x}"
        ));
    }

    let status = set(WHvPartitionPropertyCodeNestedVirtualization, 1);
    if status < 0 {
        return Err(format!(
            "nested virtualization is not offered by this Windows Hypervisor Platform (status {status:#x})"
        ));
    }

    // Setup refuses nesting unless the partition also emulates a local APIC.
    let status = set(
        WHvPartitionPropertyCodeLocalApicEmulationMode,
        WHvX64LocalApicEmulationModeXApic as u32,
    );
    if status < 0 {
        return Err(format!(
            "nested virtualization needs local APIC emulation, which the Windows Hypervisor Platform refused with status {status:#x}"
        ));
    }

    // SAFETY: `partition` is live and configured.
    let status = unsafe { (api.setup)(partition) };
    if status < 0 {
        return Err(format!(
            "nested virtualization was refused by the Windows Hypervisor Platform with status {status:#x}"
        ));
    }

    Ok(())
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

    /// Set `MSB_TEST_EXPECT_NESTED_VIRT=1` where the host supports nesting, so a check that
    /// wrongly refuses it fails instead of passing as an unsupported host.
    #[test]
    fn hosts_expected_to_nest_pass_the_check() {
        if std::env::var_os("MSB_TEST_EXPECT_NESTED_VIRT").is_some() {
            assert_eq!(super::host_support(), Ok(()));
        }
    }

    /// Every Windows check creates and deletes a partition, so repeated checks must agree
    /// instead of exhausting partitions.
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    #[test]
    fn windows_check_is_repeatable() {
        let first = super::host_support();

        for _ in 0..16 {
            assert_eq!(super::host_support(), first);
        }
    }
}
