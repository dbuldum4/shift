//! Capability consistency checks for the default conversion registry.
//!
//! These tests never invoke external converters; they only walk advertised
//! input extensions × output formats and assert `supports` / `module_for`
//! stay aligned with module capability lists.

use super::{ConversionRegistry, OutputFormat};
use std::path::PathBuf;

#[test]
fn default_registry_capability_consistency() {
    let registry = ConversionRegistry::default();
    let module_ids: Vec<&'static str> = registry.modules().map(|m| m.id()).collect();
    assert!(
        !module_ids.is_empty(),
        "default registry must register at least one module"
    );

    // One priority-boosted registry per module (avoids rebuilds inside the pair loop).
    let prioritized: Vec<ConversionRegistry> = module_ids
        .iter()
        .map(|id| ConversionRegistry::default().with_priority(&[*id]))
        .collect();

    let mut total_pairs = 0usize;
    let mut supports_true = 0usize;
    let mut module_for_hits = 0usize;
    let mut first_priority_matches = 0usize;

    for (module_index, module) in registry.modules().enumerate() {
        let inputs = module.input_extensions();
        let outputs = module.output_formats();
        assert!(
            !inputs.is_empty(),
            "module {} must advertise at least one input extension",
            module.id()
        );
        assert!(
            !outputs.is_empty(),
            "module {} must advertise at least one output format",
            module.id()
        );

        // Chainable outputs must be a subset of advertised outputs.
        for &chainable in module.chainable_output_formats() {
            assert!(
                outputs.contains(&chainable),
                "module {} chainable output {} is not in output_formats",
                module.id(),
                chainable.id()
            );
        }

        let prio_registry = &prioritized[module_index];
        let first = prio_registry
            .modules()
            .next()
            .expect("priority registry non-empty");
        assert_eq!(first.id(), module.id());

        for &ext in inputs {
            for &output in outputs {
                total_pairs += 1;
                let path = PathBuf::from(format!("sample.{ext}"));

                // Advertised input+output ⇒ supports must be true.
                assert!(
                    module.supports(&path, output),
                    "module {} lists .{ext} → {} but supports() is false",
                    module.id(),
                    output.id()
                );
                supports_true += 1;

                // Case-insensitive extension handling.
                let upper = PathBuf::from(format!("sample.{}", ext.to_ascii_uppercase()));
                assert!(
                    module.supports(&upper, output),
                    "module {} should treat .{ext} case-insensitively for {}",
                    module.id(),
                    output.id()
                );

                // Registry dispatch: some module must handle the pair.
                let chosen = registry.module_for(&path, output);
                assert!(
                    chosen.is_some(),
                    "module_for(sample.{ext}, {}) should be Some when a module advertises the pair",
                    output.id()
                );
                module_for_hits += 1;

                // When this module is first in priority and supports the pair, it wins.
                if first.supports(&path, output) {
                    let routed = prio_registry.module_for(&path, output);
                    assert!(
                        routed.is_some(),
                        "module_for under priority for {} must not panic/None for .{ext} → {}",
                        module.id(),
                        output.id()
                    );
                    assert_eq!(
                        routed.map(|m| m.id()),
                        Some(module.id()),
                        "when {} is first and supports .{ext} → {}, module_for should return it",
                        module.id(),
                        output.id()
                    );
                    first_priority_matches += 1;
                }
            }
        }

        // Extension not in the list must not support any output from this module alone.
        let foreign = PathBuf::from("sample.__shift_no_such_ext__");
        for &output in outputs {
            assert!(
                !module.supports(&foreign, output),
                "module {} should not support unknown extension for {}",
                module.id(),
                output.id()
            );
        }
    }

    // Cross-check: every OutputFormat::ALL is probed without panic.
    let mut catalog_probes = 0usize;
    for format in OutputFormat::ALL {
        for module in registry.modules() {
            if let Some(&ext) = module.input_extensions().first() {
                let path = PathBuf::from(format!("probe.{ext}"));
                let _ = module.supports(&path, *format);
                let _ = registry.module_for(&path, *format);
                catalog_probes += 1;
            }
        }
    }

    assert!(total_pairs > 0);
    assert_eq!(supports_true, total_pairs);
    assert_eq!(module_for_hits, total_pairs);
    assert!(
        first_priority_matches > 0,
        "expected at least one first-priority match"
    );
    assert!(catalog_probes > 0);

    // Record counts in the assertion message for test output / regression awareness.
    assert!(
        total_pairs >= module_ids.len(),
        "parity walk: modules={} pairs={} supports_true={} module_for_hits={} first_priority={} catalog_probes={}",
        module_ids.len(),
        total_pairs,
        supports_true,
        module_for_hits,
        first_priority_matches,
        catalog_probes
    );
}

#[test]
fn default_registry_module_ids_are_unique_and_stable() {
    let registry = ConversionRegistry::default();
    let ids: Vec<&str> = registry.modules().map(|m| m.id()).collect();
    assert_eq!(
        ids,
        vec!["markitdown", "pandoc", "defuddle", "docling", "ffmpeg"]
    );
    let mut seen = std::collections::HashSet::new();
    for id in &ids {
        assert!(seen.insert(*id), "duplicate module id: {id}");
        assert!(!id.is_empty());
    }
}
