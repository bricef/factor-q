use super::*;

#[test]
fn render_recovery_guidance_all_clear() {
    let out = render_recovery_guidance(0, 0);
    assert!(out.contains("All clear"), "got: {out:?}");
    // No command hints when nothing's pending.
    assert!(
        !out.contains("fq invocation"),
        "should not hint commands: {out:?}"
    );
    assert!(
        !out.contains("fq workers"),
        "should not hint commands: {out:?}"
    );
}

#[test]
fn render_recovery_guidance_for_ambiguous_only() {
    let out = render_recovery_guidance(3, 0);
    assert!(out.contains("Ambiguous invocations: 3"));
    assert!(out.contains("fq invocation list --status=ambiguous"));
    assert!(out.contains("fq invocation drop"));
    assert!(!out.contains("Stale workers"), "got: {out:?}");
    assert!(!out.contains("All clear"));
}

#[test]
fn render_recovery_guidance_for_stale_only() {
    let out = render_recovery_guidance(0, 2);
    assert!(out.contains("Stale workers: 2"));
    assert!(out.contains("fq workers list --stale-only"));
    // Inspection is offered; removal is not. The retired `fq workers
    // prune` must not come back as advice.
    assert!(!out.contains("prune"), "got: {out:?}");
    assert!(out.contains("retention sweep"));
    assert!(!out.contains("Ambiguous"), "got: {out:?}");
    assert!(!out.contains("All clear"));
}

#[test]
fn render_recovery_guidance_for_both() {
    let out = render_recovery_guidance(1, 1);
    assert!(out.contains("Ambiguous invocations: 1"));
    assert!(out.contains("Stale workers: 1"));
    assert!(out.contains("fq invocation drop"));
    assert!(out.contains("fq workers list --stale-only"));
    assert!(!out.contains("prune"), "got: {out:?}");
}
