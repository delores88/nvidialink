# Notes for the PhaseForge owner

Read-only observations against `delores88/PhaseForge` commit
`ac30e04fbf0ddc03ae0144fb06b6350cacdce1fc`. All execution used an isolated tracked
archive in Spark Dev Bridge's own workspace and its own containers/caches. No
PhaseForge checkout, service, image, volume, or build directory was changed.

## ARM64 file-open flags

`backend/src/laboratory/nr_retention.rs:169` passes hardcoded `0x20000 | 0x10000`
as `O_NOFOLLOW | O_DIRECTORY`. Lines 193, 219, 251 and 288 also pass `0x20000`
as `O_NOFOLLOW`. These literals are Linux x86 values. On the physical aarch64
Spark, `O_NOFOLLOW=0x8000` and `O_DIRECTORY=0x4000`; `0x10000` is `O_DIRECT`.
A read-only `os.open('/tmp', ...)` probe reproduces errno 22 using the literals
and succeeds with the platform constants. See
[probe receipt](remote/receipts/aarch64-open-flags.json).

Use target-specific OS constants in the upstream implementation and review every
hardcoded Unix flag. The current literal also does not request `O_NOFOLLOW` on
ARM64; this observation alone is not an exploit demonstration. We have not patched
or rerun a modified PhaseForge build, so the exact number fixed by that change
remains unverified. The full-suite failures below share this directory-opening
path or depend on a retained result it failed to create:

- `agent::laboratory::intent::black_hole::tests::ordinary_null_capability_analysis_reads_verified_puncture_measurements_but_collision_simulation_cannot`
- `laboratory::nr_retention::tests::black_hole_changed_native_measurements_or_source_identity_cannot_replace_saved_result`
- `laboratory::nr_retention::tests::black_hole_execution_and_measurements_never_upgrade_unvalidated_recovery`
- `laboratory::nr_retention::tests::changed_staged_request_and_changed_retained_output_are_not_silently_replaced`
- `laboratory::nr_retention::tests::completed_black_hole_readonly_verifier_never_imports_or_mutates_job_or_files`
- `laboratory::nr_retention::tests::exact_native_bytes_and_terminal_receipt_import_idempotently_without_runtime_request`
- `laboratory::nr_retention::tests::failed_stream_pin_never_publishes_partial_final_file`
- `laboratory::nr_retention::tests::large_native_restart_import_is_streamed_and_published_idempotently`
- `laboratory::nr_retention::tests::new_explicit_recovery_after_completion_reverifies_and_records_tamper_failure`
- `laboratory::nr_retention::tests::only_verified_analytic_success_can_complete_and_analytic_failure_stays_failed`
- `laboratory::nr_retention::tests::over_budget_inventory_retains_terminal_facts_but_does_not_copy_or_complete`
- `laboratory::nr_retention::tests::prior_conflicting_local_file_is_preserved_and_missing_files_can_be_retried`
- `laboratory::nr_retention::tests::readonly_diagnostic_evidence_requires_original_admission_and_completed_state`
- `laboratory::nr_retention::tests::recovery_cancellation_removes_only_unpublished_partial_copy_and_resets_thread_context`
- `laboratory::nr_retention::tests::rehashed_black_hole_summary_cannot_assert_boost_or_merger_or_other_execution`
- `laboratory::nr_retention::tests::retained_monitor_uses_frozen_historical_sources_and_rejects_changed_receipts`
- `laboratory::nr_retention::tests::stopped_or_nonzero_attempts_keep_exact_partial_data_without_certifying_completion`

## Platform requirement and our scratch limit

`laboratory::generated::spark::tests::spark_explicit_recovery_gets_same_finished_job_without_reexecution`
fails at `generated_spark_tests.rs:53` with “Validated generated-code storage
monitoring requires Windows”. This Linux result does not establish a Windows bug.

`laboratory::generated::tests::registered_large_original_streams_both_charged_copies_and_restarts_from_pinned_snapshot`
failed at `generated.rs:1837` with errno 28 under Spark Dev Bridge's initial
512 MiB `/tmp`. We increased only this benchmark's tmpfs grant to 4 GiB, still
charged against its 32 GiB memory cap. The original three timing receipts remain
in [the benchmark receipt](remote/receipts/phaseforge-benchmark.json). The full
corrected-policy rerun is recorded separately in
[the corrected scratch receipt](remote/receipts/phaseforge-scratch-corrected.json).
That complete rerun passed the large-original test and finished with **1969 passed,
18 failed, 75 ignored**, exit 101, in 80.68 seconds. Its remaining failure names
are exactly the 17 retention-related tests above and the Windows-requiring test.
The job was an actual execution (`cache_hit:false`), with 8 CPUs / 32 GiB / 4 GiB
scratch; no PhaseForge source change was applied to obtain this result.

Initial totals, including integration tests: 1968 passed, 19 failed, 75 ignored.
Cargo-reported compile times: cold 1m 42s, unchanged 0.16s, one-file change 24.04s.
Full job execution times were 183.16s, 80.73s and 104.54s respectively; these are
not successful-suite performance claims. The cold target cache started empty;
locked dependencies had already been prepared. The one-file change was a comment
in the isolated `backend/src/main.rs`, restored afterward.
