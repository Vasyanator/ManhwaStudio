/*
File: torch/tests.rs

Purpose:
Golden characterization of the Torch seam — the exact option set, labels and `pip install`
arguments (CPU / CUDA recorded against the pre-extraction code, ROCm against the verified AMD
whl-next resolve) — plus the option builder, uninstall set, readiness, record store,
constraints and driver-note tables. Literal expectations on purpose: a pin, flag, label or URL
change must show up here as a deliberate test edit.
*/

use std::collections::HashMap;

use ms_config::torch_choice::{GfxArch, TorchChoiceRecord, TorchFlavour};
use ms_sysprobe::amd_adapters::{AMD_PCI_VENDOR_ID, AmdAdapter, GfxSource};
use ms_sysprobe::gpu_utils::RuntimeVersion;

use super::catalog::{
    build_cuda_torch_options, build_rocm_torch_option, choose_recommended_torch_option, driver_note, rocm_device_extras,
    rocm_download_estimate,
};
use super::choice_store::persist_torch_choice_record;
use super::command::{TorchCommandError, amd_packages_to_remove, torch_install_pip_args};
use super::constraints::{torch_constraints_content, write_torch_constraints};
use super::preflight::{GpuFacts, NvidiaFacts, build_torch_options};
use super::{
    DriverNote, TorchInstallSelection, TorchOs, TorchReadiness, TorchWheelOption, installed_torch_is_current,
    installed_torch_version, read_torch_choice_record, required_torch_version, torch_readiness,
};

/// Installs the embedded English catalog so localized texts are rendered (the empty default
/// catalog returns raw keys and leaves placeholders unfilled). Idempotent and process-global,
/// as in the other crates' tests.
fn english_catalog() {
    ms_i18n::set_locale(&ms_i18n::LocaleTag::english()).expect("the embedded English catalog installs");
}

fn gfx(name: &str) -> GfxArch {
    GfxArch::parse(name).unwrap_or_else(|| panic!("{name} must parse"))
}

/// The observable identity of an option: flavour, wheel tag, label and gating version.
fn summary(options: &[TorchWheelOption]) -> Vec<(TorchFlavour, &str, &str, RuntimeVersion)> {
    options
        .iter()
        .map(|option| (option.flavour, option.wheel_tag.as_str(), option.label.as_str(), option.version))
        .collect()
}

/// A discrete / integrated / unknown AMD adapter for the option builder.
fn adapter(device_id: u16, gfx_name: Option<&str>, integrated: Option<bool>, driver: Option<&str>) -> AmdAdapter {
    AmdAdapter {
        vendor_id: AMD_PCI_VENDOR_ID,
        device_id,
        revision: None,
        name: format!("Radeon {device_id:04x}"),
        gfx: gfx_name.map(gfx),
        gfx_source: gfx_name.map(|_| GfxSource::DeviceIdTable),
        integrated,
        driver_version: driver.map(str::to_string),
    }
}

fn rocm_option(os: TorchOs, gfx_name: &str) -> TorchWheelOption {
    build_rocm_torch_option(os, gfx(gfx_name), vec![adapter(0x7550, Some(gfx_name), Some(false), None)], Some("32.0.31041.5005"))
}

fn args(selection: &TorchInstallSelection) -> Vec<String> {
    torch_install_pip_args(selection).unwrap_or_else(|err| panic!("{selection:?} must build: {err:?}"))
}

#[test]
fn cpu_install_args_are_pinned_pypi_specs() {
    assert_eq!(
        args(&TorchInstallSelection::SkipCpu),
        vec!["--force-reinstall", "--no-cache-dir", "torch==2.9.1", "torchvision==0.24.1"]
    );
}

#[test]
fn cuda_install_args_add_the_wheel_index_of_the_option() {
    let options = build_cuda_torch_options(RuntimeVersion::new(13, 0), Some(RuntimeVersion::new(8, 9)));
    let all_args = options.into_iter().map(|option| args(&TorchInstallSelection::for_option(option))).collect::<Vec<_>>();
    let expected = ["cu126", "cu128", "cu130"]
        .map(|tag| {
            vec![
                "--force-reinstall".to_string(),
                "--no-cache-dir".to_string(),
                "torch==2.9.1".to_string(),
                "torchvision==0.24.1".to_string(),
                "--index-url".to_string(),
                format!("https://download.pytorch.org/whl/{tag}"),
            ]
        })
        .to_vec();
    assert_eq!(all_args, expected);
}

#[test]
fn rocm_install_args_use_the_amd_index_alone_with_the_device_extra() {
    let option = rocm_option(TorchOs::Windows, "gfx1201");
    assert_eq!(
        args(&TorchInstallSelection::for_option(option.clone())),
        vec![
            "--index-url",
            "https://stable.repo.amd.com/rocm/whl-next/",
            "torch[device-gfx1201]==2.13.0+rocm10.1.0",
            "torchvision[device-gfx1201]==0.28.0+rocm10.1.0",
        ]
    );
    // A user override inside the supported list rebuilds the specs for that architecture.
    assert_eq!(
        args(&TorchInstallSelection::InstallRocm { option, gfx: gfx("gfx1100") }),
        vec![
            "--index-url",
            "https://stable.repo.amd.com/rocm/whl-next/",
            "torch[device-gfx1100]==2.13.0+rocm10.1.0",
            "torchvision[device-gfx1100]==0.28.0+rocm10.1.0",
        ]
    );
}

#[test]
fn invalid_selections_are_rejected_before_any_command() {
    let rocm = rocm_option(TorchOs::Windows, "gfx1201");
    assert_eq!(
        torch_install_pip_args(&TorchInstallSelection::InstallRocm { option: rocm.clone(), gfx: gfx("gfx803") }),
        Err(TorchCommandError::UnsupportedArch { gfx: gfx("gfx803") })
    );
    // gfx942 has device wheels on Linux only.
    assert_eq!(
        torch_install_pip_args(&TorchInstallSelection::InstallRocm { option: rocm.clone(), gfx: gfx("gfx942") }),
        Err(TorchCommandError::UnsupportedArch { gfx: gfx("gfx942") })
    );
    assert_eq!(torch_install_pip_args(&TorchInstallSelection::InstallGpu(rocm)), Err(TorchCommandError::NotACudaOption));
    let cuda = build_cuda_torch_options(RuntimeVersion::new(12, 6), None).remove(0);
    assert_eq!(
        torch_install_pip_args(&TorchInstallSelection::InstallRocm { option: cuda, gfx: gfx("gfx1201") }),
        Err(TorchCommandError::NotARocmOption)
    );
}

#[test]
fn selection_records_the_choice() {
    assert_eq!(TorchInstallSelection::SkipCpu.record(), TorchChoiceRecord::Cpu);
    let cuda = build_cuda_torch_options(RuntimeVersion::new(12, 6), None).remove(0);
    let cuda_selection = TorchInstallSelection::for_option(cuda);
    assert!(cuda_selection.is_cuda());
    assert_eq!(cuda_selection.record(), TorchChoiceRecord::Cuda);
    let rocm_selection = TorchInstallSelection::for_option(rocm_option(TorchOs::Linux, "gfx1100"));
    assert!(!rocm_selection.is_cuda());
    assert_eq!(rocm_selection.record(), TorchChoiceRecord::Rocm { gfx: gfx("gfx1100") });
}

#[test]
fn cuda_options_follow_runtime_and_compute_capability() {
    assert_eq!(
        summary(&build_cuda_torch_options(RuntimeVersion::new(13, 0), Some(RuntimeVersion::new(8, 9)))),
        vec![
            (TorchFlavour::Cuda, "cu126", "CUDA 12.6 (cu126)", RuntimeVersion::new(12, 6)),
            (TorchFlavour::Cuda, "cu128", "CUDA 12.8 (cu128)", RuntimeVersion::new(12, 8)),
            (TorchFlavour::Cuda, "cu130", "CUDA 13.0 (cu130)", RuntimeVersion::new(13, 0)),
        ]
    );
    assert_eq!(
        summary(&build_cuda_torch_options(RuntimeVersion::new(13, 0), Some(RuntimeVersion::new(7, 0)))),
        vec![(TorchFlavour::Cuda, "cu126", "CUDA 12.6 (cu126)", RuntimeVersion::new(12, 6))]
    );
    assert!(build_cuda_torch_options(RuntimeVersion::new(13, 0), Some(RuntimeVersion::new(6, 0))).is_empty());
    assert_eq!(
        summary(&build_cuda_torch_options(RuntimeVersion::new(12, 6), None)),
        vec![(TorchFlavour::Cuda, "cu126", "CUDA 12.6 (cu126)", RuntimeVersion::new(12, 6))]
    );
    for option in build_cuda_torch_options(RuntimeVersion::new(13, 0), None) {
        assert!(option.rocm.is_none() && option.approx_download_bytes.is_none() && option.download_size_text().is_none());
    }
}

#[test]
fn rocm_option_carries_its_facts() {
    english_catalog();
    let option = rocm_option(TorchOs::Windows, "gfx1201");
    assert_eq!(option.flavour, TorchFlavour::Rocm);
    assert_eq!(option.wheel_tag, "rocm10.1.0");
    assert_eq!(option.index_url, "https://stable.repo.amd.com/rocm/whl-next/");
    assert_eq!(option.torch_spec, "torch[device-gfx1201]==2.13.0+rocm10.1.0");
    assert!(option.label.contains("gfx1201") && option.label.contains("2.13.0"), "{}", option.label);
    assert_eq!(option.approx_download_bytes, Some(1_550_000_000));
    assert!(option.download_size_text().is_some());
    let rocm = option.rocm.as_ref().expect("a ROCm option carries ROCm facts");
    assert_eq!(rocm.default_gfx, gfx("gfx1201"));
    assert_eq!(rocm.supported, rocm_device_extras(TorchOs::Windows));
    assert_eq!(rocm.driver_note, DriverNote::NotApplicable);
    assert_eq!(rocm.detected.len(), 1);
    // An unmeasured family still says "size unknown" instead of nothing.
    let unknown = rocm_option(TorchOs::Windows, "gfx1151");
    assert_eq!(unknown.approx_download_bytes, None);
    assert!(unknown.download_size_text().is_some());
}

#[test]
fn device_extras_per_os() {
    let windows = rocm_device_extras(TorchOs::Windows);
    let linux = rocm_device_extras(TorchOs::Linux);
    assert_eq!(windows.len(), 22);
    assert!(windows.iter().all(|name| linux.contains(name)), "Linux is a superset of Windows");
    for linux_only in ["gfx1250", "gfx942", "gfx950"] {
        assert!(linux.contains(&linux_only) && !windows.contains(&linux_only), "{linux_only}");
    }
    for name in windows.iter().chain(linux) {
        assert!(GfxArch::parse(name).is_some_and(|parsed| parsed.as_str() == *name), "{name} must be a processor name");
    }
}

#[test]
fn download_estimate_table() {
    let cases = [
        (TorchOs::Windows, "gfx1201", Some(1_550_000_000)),
        (TorchOs::Linux, "gfx1200", Some(1_890_000_000)),
        (TorchOs::Windows, "gfx1100", Some(1_570_000_000)),
        (TorchOs::Linux, "gfx1103", Some(1_910_000_000)),
        (TorchOs::Windows, "gfx1030", Some(1_110_000_000)),
        (TorchOs::Linux, "gfx1012", Some(1_840_000_000)),
        (TorchOs::Windows, "gfx1151", None),
        (TorchOs::Linux, "gfx90a", None),
        (TorchOs::Linux, "gfx1250", None),
    ];
    for (os, name, expected) in cases {
        assert_eq!(rocm_download_estimate(os, &gfx(name)), expected, "{os:?} {name}");
    }
}

#[test]
fn driver_note_table() {
    english_catalog();
    assert_eq!(driver_note(TorchOs::Windows, Some("32.0.31041.5004")), DriverNote::BelowMinimum { installed: "32.0.31041.5004".to_string() });
    assert_eq!(driver_note(TorchOs::Windows, Some("31.0.24033.1003")), DriverNote::BelowMinimum { installed: "31.0.24033.1003".to_string() });
    assert_eq!(driver_note(TorchOs::Windows, Some("32.0.31041.5005")), DriverNote::NotApplicable);
    assert_eq!(driver_note(TorchOs::Windows, Some("32.0.31100.1")), DriverNote::NotApplicable);
    assert_eq!(driver_note(TorchOs::Windows, Some("not a version")), DriverNote::Requirement);
    assert_eq!(driver_note(TorchOs::Windows, None), DriverNote::Requirement);
    assert_eq!(driver_note(TorchOs::Linux, None), DriverNote::NotApplicable);
    assert!(DriverNote::NotApplicable.message().is_none());
    assert!(DriverNote::Requirement.message().is_some());
    assert!(DriverNote::BelowMinimum { installed: "31.0.1.1".to_string() }.message().is_some_and(|text| text.contains("31.0.1.1")));
}

#[test]
fn recommendation_prefers_cuda_over_rocm() {
    let mut options = vec![rocm_option(TorchOs::Linux, "gfx1201")];
    options.extend(build_cuda_torch_options(RuntimeVersion::new(13, 0), Some(RuntimeVersion::new(8, 9))));
    assert_eq!(options[choose_recommended_torch_option(&options)].wheel_tag, "cu130");
    assert_eq!(choose_recommended_torch_option(&[rocm_option(TorchOs::Linux, "gfx1201")]), 0);
    assert_eq!(choose_recommended_torch_option(&[]), 0);
}

fn facts(os: TorchOs, nvidia: Option<NvidiaFacts>, amd_adapters: Vec<AmdAdapter>, linux_kfd: bool) -> GpuFacts {
    GpuFacts { os, nvidia, amd_adapters, linux_kfd }
}

#[test]
fn builder_offers_rocm_for_a_supported_windows_adapter() {
    let plan = build_torch_options(&facts(
        TorchOs::Windows,
        None,
        vec![adapter(0x7550, Some("gfx1201"), Some(false), Some("32.0.31041.5005"))],
        false,
    ));
    assert!(plan.failures.is_empty(), "{:?}", plan.failures);
    assert_eq!(plan.options.len(), 1);
    assert_eq!(plan.options[0].rocm.as_ref().map(|rocm| rocm.default_gfx.clone()), Some(gfx("gfx1201")));
}

#[test]
fn builder_prefers_the_discrete_adapter() {
    // `detect_amd_adapters` orders discrete first; the builder takes the first one.
    let plan = build_torch_options(&facts(
        TorchOs::Windows,
        None,
        vec![adapter(0x744c, Some("gfx1100"), Some(false), None), adapter(0x15bf, Some("gfx1103"), Some(true), None)],
        false,
    ));
    let rocm = plan.options[0].rocm.as_ref().expect("ROCm option");
    assert_eq!(rocm.default_gfx, gfx("gfx1100"));
    assert_eq!(rocm.detected.len(), 2);
    assert_eq!(rocm.driver_note, DriverNote::Requirement);
}

#[test]
fn builder_explains_every_missing_rocm_option() {
    english_catalog();
    let unsupported = build_torch_options(&facts(TorchOs::Windows, None, vec![adapter(0x67df, Some("gfx803"), Some(false), None)], false));
    assert!(unsupported.options.is_empty());
    assert_eq!(unsupported.failures.len(), 1);
    assert!(unsupported.failures[0].contains("gfx803"), "{:?}", unsupported.failures);

    let no_kfd = build_torch_options(&facts(TorchOs::Linux, None, vec![adapter(0x7550, Some("gfx1201"), Some(false), None)], false));
    assert!(no_kfd.options.is_empty());
    assert_eq!(no_kfd.failures.len(), 1);

    let with_kfd = build_torch_options(&facts(TorchOs::Linux, None, vec![adapter(0x7550, Some("gfx1201"), Some(false), None)], true));
    assert_eq!(with_kfd.options.len(), 1);
    assert!(with_kfd.failures.is_empty());

    let unknown = build_torch_options(&facts(TorchOs::Windows, None, vec![adapter(0x7abc, None, None, None)], false));
    assert!(unknown.options.is_empty());
    assert_eq!(unknown.failures.len(), 1);
    assert!(unknown.failures[0].contains("7abc"), "the reason names the hex device id: {:?}", unknown.failures);
}

#[test]
fn builder_recommends_cuda_on_a_mixed_machine() {
    let plan = build_torch_options(&facts(
        TorchOs::Linux,
        Some(NvidiaFacts { cuda_version: Some(RuntimeVersion::new(12, 8)), capability: Some(RuntimeVersion::new(8, 9)) }),
        vec![adapter(0x7550, Some("gfx1201"), Some(false), None)],
        true,
    ));
    assert_eq!(plan.options.len(), 3, "cu126 + cu128 + ROCm");
    let recommended = &plan.options[choose_recommended_torch_option(&plan.options)];
    assert_eq!(recommended.wheel_tag, "cu128");
    assert!(plan.options.iter().any(|option| option.flavour == TorchFlavour::Rocm));
}

/// Builds a `pip freeze` style snapshot from `name==version` pairs. The keys are used as
/// given, so callers pass already-normalized names (`torch`).
fn installed_map(entries: &[(&str, &str)]) -> HashMap<String, String> {
    entries.iter().map(|(name, version)| ((*name).to_string(), (*version).to_string())).collect()
}

fn rocm_env(arch: &str) -> HashMap<String, String> {
    installed_map(&[
        ("torch", "2.13.0+rocm10.1.0"),
        ("torchvision", "0.28.0+rocm10.1.0"),
        (&format!("amd-torch-device-{arch}"), "2.13.0+rocm10.1.0"),
        ("amd-torch-device-gfx12-0", "2.13.0+rocm10.1.0"),
        (&format!("amd-torchvision-device-{arch}"), "0.28.0+rocm10.1.0"),
        (&format!("rocm-sdk-device-{arch}"), "10.1.0"),
        ("rocm-sdk-core", "10.1.0"),
        ("rocm-sdk-libraries", "10.1.0"),
        ("rocm", "10.1.0"),
        ("rocm-bootstrap", "0.3.0"),
        ("triton", "3.8.0+git669b31ac.rocm10.1.0"),
        ("numpy", "2.2.6"),
    ])
}

fn removal(installed: &HashMap<String, String>, target: &TorchChoiceRecord) -> Vec<String> {
    amd_packages_to_remove(installed, target)
}

#[test]
fn uninstall_set_for_a_non_rocm_target_removes_every_amd_package() {
    let mut installed = rocm_env("gfx1201");
    installed.insert("pytorch-triton-rocm".to_string(), "3.4.0".to_string());
    installed.insert("rocm-profiler".to_string(), "1.0".to_string());
    let expected = vec![
        "amd-torch-device-gfx12-0", "amd-torch-device-gfx1201", "amd-torchvision-device-gfx1201", "pytorch-triton-rocm", "rocm",
        "rocm-bootstrap", "rocm-profiler", "rocm-sdk-core", "rocm-sdk-device-gfx1201", "rocm-sdk-libraries", "triton",
    ];
    for target in [TorchChoiceRecord::Cpu, TorchChoiceRecord::Cuda] {
        assert_eq!(removal(&installed, &target), expected, "{target:?}");
    }
    // A CUDA environment loses nothing: its triton is not a ROCm build.
    let cuda_env = installed_map(&[("torch", "2.9.1+cu126"), ("triton", "3.5.1"), ("numpy", "2.2.6")]);
    assert!(removal(&cuda_env, &TorchChoiceRecord::Cpu).is_empty());
}

#[test]
fn uninstall_set_for_a_rocm_target() {
    let same = TorchChoiceRecord::Rocm { gfx: gfx("gfx1201") };
    // Same architecture: exactly-`gfx1201` packages stay; the family wheel always goes (the
    // resolve re-pulls it from uv's cache).
    assert_eq!(removal(&rocm_env("gfx1201"), &same), vec!["amd-torch-device-gfx12-0"]);
    // Stale family and sibling-arch packages from an earlier install never survive a
    // same-gfx reinstall.
    let mut mixed = rocm_env("gfx1201");
    for (name, version) in [
        ("amd-torch-device-gfx110x", "2.13.0+rocm10.1.0"),
        ("amd-torch-device-gfx1100", "2.13.0+rocm10.1.0"),
        ("amd-torchvision-device-gfx1100", "0.28.0+rocm10.1.0"),
        ("rocm-sdk-device-gfx1100", "10.1.0"),
    ] {
        mixed.insert(name.to_string(), version.to_string());
    }
    assert_eq!(
        removal(&mixed, &same),
        vec![
            "amd-torch-device-gfx1100", "amd-torch-device-gfx110x", "amd-torch-device-gfx12-0", "amd-torchvision-device-gfx1100",
            "rocm-sdk-device-gfx1100",
        ]
    );

    // Legacy pytorch.org ROCm triton always goes; the AMD triton stays for a ROCm target.
    let mut legacy = installed_map(&[("torch", "2.9.1+rocm6.4"), ("pytorch-triton-rocm", "3.4.0"), ("triton-rocm", "3.1.0")]);
    assert_eq!(removal(&legacy, &same), vec!["pytorch-triton-rocm", "triton-rocm"]);
    legacy.insert("triton".to_string(), "3.8.0+git669b31ac.rocm10.1.0".to_string());
    assert_eq!(removal(&legacy, &same), vec!["pytorch-triton-rocm", "triton-rocm"]);

    // Architecture change: every device package goes, the shared runtime stays.
    let changed = TorchChoiceRecord::Rocm { gfx: gfx("gfx1100") };
    assert_eq!(
        removal(&rocm_env("gfx1201"), &changed),
        vec!["amd-torch-device-gfx12-0", "amd-torch-device-gfx1201", "amd-torchvision-device-gfx1201", "rocm-sdk-device-gfx1201"]
    );

    // A clean CUDA environment needs nothing removed for ROCm.
    assert!(removal(&installed_map(&[("torch", "2.9.1+cu126"), ("triton", "3.5.1")]), &same).is_empty());
}

#[test]
fn readiness_unrecorded_keeps_the_legacy_version_rule() {
    let record = TorchChoiceRecord::Unrecorded;
    assert_eq!(torch_readiness(&installed_map(&[]), &record), TorchReadiness::Missing);
    assert_eq!(
        torch_readiness(&installed_map(&[("torch", "1.13.1")]), &record),
        TorchReadiness::Outdated { installed: "1.13.1".to_string(), required: "2.9.1".to_string() }
    );
    for version in ["2.9.1", "2.12.0", "2.12.0+rocm7.2", "2.9.1+rocm6.4", "2.9.1+cu126", "2.13.0+rocm10.1.0"] {
        assert!(installed_torch_is_current(&installed_map(&[("torch", version)]), &record), "{version}");
    }
    assert!(
        !installed_torch_is_current(&installed_map(&[("torch", "not-a-version")]), &record),
        "an unreadable version must fail closed, never pass as current"
    );
    assert_eq!(installed_torch_version(&installed_map(&[("torch", "2.9.1")])), Some("2.9.1"));
    assert_eq!(installed_torch_version(&installed_map(&[])), None);
}

#[test]
fn readiness_cpu_and_cuda_check_the_flavour_tag() {
    let cpu = TorchChoiceRecord::Cpu;
    assert!(installed_torch_is_current(&installed_map(&[("torch", "2.9.1")]), &cpu));
    assert!(installed_torch_is_current(&installed_map(&[("torch", "2.9.1+cpu")]), &cpu));
    assert_eq!(
        torch_readiness(&installed_map(&[("torch", "2.9.1+cu126")]), &cpu),
        TorchReadiness::FlavourMismatch { installed: Some(TorchFlavour::Cuda), expected: TorchFlavour::Cpu }
    );
    assert_eq!(
        torch_readiness(&installed_map(&[("torch", "2.8.0+cpu")]), &cpu),
        TorchReadiness::Outdated { installed: "2.8.0+cpu".to_string(), required: "2.9.1".to_string() }
    );

    let cuda = TorchChoiceRecord::Cuda;
    assert!(installed_torch_is_current(&installed_map(&[("torch", "2.9.1+cu128")]), &cuda));
    assert_eq!(
        torch_readiness(&installed_map(&[("torch", "2.9.1")]), &cuda),
        TorchReadiness::FlavourMismatch { installed: None, expected: TorchFlavour::Cuda }
    );
    assert_eq!(
        torch_readiness(&installed_map(&[("torch", "2.13.0+rocm10.1.0")]), &cuda),
        TorchReadiness::FlavourMismatch { installed: Some(TorchFlavour::Rocm), expected: TorchFlavour::Cuda }
    );
    assert_eq!(torch_readiness(&installed_map(&[]), &cuda), TorchReadiness::Missing);
    assert_eq!(required_torch_version(&cuda), "2.9.1");
}

#[test]
fn readiness_rocm_needs_exact_pins_and_device_packages() {
    let record = TorchChoiceRecord::Rocm { gfx: gfx("gfx1201") };
    assert_eq!(required_torch_version(&record), "2.13.0+rocm10.1.0");
    assert!(installed_torch_is_current(&rocm_env("gfx1201"), &record));
    assert_eq!(torch_readiness(&installed_map(&[]), &record), TorchReadiness::Missing);
    assert_eq!(
        torch_readiness(&installed_map(&[("torch", "2.9.1+cu126")]), &record),
        TorchReadiness::FlavourMismatch { installed: Some(TorchFlavour::Cuda), expected: TorchFlavour::Rocm }
    );
    // The legacy pytorch.org ROCm build is outdated against the recorded AMD pin.
    assert_eq!(
        torch_readiness(&installed_map(&[("torch", "2.9.1+rocm6.4")]), &record),
        TorchReadiness::Outdated { installed: "2.9.1+rocm6.4".to_string(), required: "2.13.0+rocm10.1.0".to_string() }
    );
    let mut wrong_vision = rocm_env("gfx1201");
    wrong_vision.insert("torchvision".to_string(), "0.29.0a0+rocm10.1.0".to_string());
    assert!(matches!(torch_readiness(&wrong_vision, &record), TorchReadiness::Outdated { .. }));
    let mut no_vision = rocm_env("gfx1201");
    no_vision.remove("torchvision");
    assert_eq!(torch_readiness(&no_vision, &record), TorchReadiness::Missing);
    // A bare torch from the AMD index has no kernels: the device packages are required.
    assert_eq!(torch_readiness(&rocm_env("gfx1100"), &record), TorchReadiness::DevicePackagesMissing { gfx: gfx("gfx1201") });
    let mut no_core = rocm_env("gfx1201");
    no_core.remove("rocm-sdk-core");
    assert_eq!(torch_readiness(&no_core, &record), TorchReadiness::DevicePackagesMissing { gfx: gfx("gfx1201") });
    for readiness in [
        TorchReadiness::Missing,
        TorchReadiness::FlavourMismatch { installed: None, expected: TorchFlavour::Rocm },
        TorchReadiness::DevicePackagesMissing { gfx: gfx("gfx1201") },
    ] {
        assert!(readiness.user_message("2.13.0+rocm10.1.0").is_some(), "{readiness:?}");
    }
    assert!(TorchReadiness::Current.user_message("2.9.1").is_none());
}

#[test]
fn record_store_round_trips_and_keeps_malformed_documents() {
    let dir = tempfile::tempdir().expect("temp dir");
    assert_eq!(read_torch_choice_record(dir.path()), TorchChoiceRecord::Unrecorded, "absent document");

    let rocm = TorchChoiceRecord::Rocm { gfx: gfx("gfx1201") };
    persist_torch_choice_record(dir.path(), &rocm).expect("record must be written");
    assert_eq!(read_torch_choice_record(dir.path()), rocm);
    persist_torch_choice_record(dir.path(), &TorchChoiceRecord::Cuda).expect("record must be overwritten");
    assert_eq!(read_torch_choice_record(dir.path()), TorchChoiceRecord::Cuda);

    let malformed_dir = tempfile::tempdir().expect("temp dir");
    let path = malformed_dir.path().join(ms_config::USER_CONFIG_FILE);
    std::fs::write(&path, "{ not json").expect("fixture must be writable");
    assert_eq!(read_torch_choice_record(malformed_dir.path()), TorchChoiceRecord::Unrecorded);
    assert!(persist_torch_choice_record(malformed_dir.path(), &rocm).is_err());
    assert_eq!(std::fs::read_to_string(&path).expect("fixture readable"), "{ not json", "a malformed document is never overwritten");
}

#[test]
fn constraints_pin_the_installed_torch() {
    assert_eq!(torch_constraints_content(&installed_map(&[("numpy", "2.2.6")])), None);
    assert_eq!(
        torch_constraints_content(&installed_map(&[("torch", "2.9.1+cu126")])).as_deref(),
        Some("torch==2.9.1+cu126\n")
    );
    let installed = rocm_env("gfx1201");
    assert_eq!(
        torch_constraints_content(&installed).as_deref(),
        Some("torch==2.13.0+rocm10.1.0\ntorchvision==0.28.0+rocm10.1.0\n")
    );

    let dir = tempfile::tempdir().expect("temp dir");
    assert_eq!(write_torch_constraints(dir.path(), &installed_map(&[])).expect("no torch is not an error"), None);
    assert!(!dir.path().join("installer_files").exists(), "nothing is written without torch");
    let path = write_torch_constraints(dir.path(), &installed).expect("write").expect("a torch gives a file");
    assert_eq!(path, dir.path().join("installer_files").join("torch-constraints.txt"));
    assert_eq!(
        std::fs::read_to_string(&path).expect("constraints readable"),
        "torch==2.13.0+rocm10.1.0\ntorchvision==0.28.0+rocm10.1.0\n"
    );
}
