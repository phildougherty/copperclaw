# Observability

Copperclaw exposes two opt-in observability surfaces, both off by default
per the conservative-defaults tenet. Enable them by setting the
relevant environment variable before launching `copperclaw run`.

## Prometheus metrics endpoint

Set `COPPERCLAW_METRICS_ADDR` to start a small `/metrics` HTTP server on
boot. Accepts either a full `host:port` pair or a bare port (which
auto-prefixes to `127.0.0.1:` so a typo never opens a public listener
by accident).

```
# Loopback only — bare port shorthand.
export COPPERCLAW_METRICS_ADDR=9090

# Or explicit host:port.
export COPPERCLAW_METRICS_ADDR=127.0.0.1:9090

# All interfaces (do this only when the port is behind a reverse proxy
# that enforces authn/authz).
export COPPERCLAW_METRICS_ADDR=0.0.0.0:9090
```

Bind failures log a warning and the host continues to boot — metrics
is never a hard dependency. Malformed addresses log
`COPPERCLAW_METRICS_ADDR is malformed, metrics endpoint disabled: ...`
and the endpoint stays off; the rest of the host runs normally.

### What is exported

~160 metric families, all prefixed `copperclaw_`. Every family is
registered through a named helper in
`crates/copperclaw-metrics/src/lib.rs` — that file is the
authoritative registry; the tables below group the families by
subsystem. Naming follows the Prometheus convention: `_total`
families are counters; `_seconds` / `_bytes` / count-shaped families
are histograms; gauges are marked in the tables.

#### Message pipeline (router + delivery)

| Name | Labels | Meaning |
|---|---|---|
| `copperclaw_messages_inbound_total` | `channel_type` | Router, after a successful inbound DB write. |
| `copperclaw_messages_outbound_total` | `channel_type` | Delivery loop, on successful adapter dispatch. |
| `copperclaw_delivery_failed_total` | `channel_type` | Final delivery failure (3 retries exhausted). |
| `copperclaw_delivery_formatting_fallback_total` | `channel_type` | Rich formatting failed; delivery fell back to plain text. |
| `copperclaw_delivery_chat_split_total` | `channel_type` | A reply was split across multiple chat messages. |
| `copperclaw_delivery_fence_split_total` | `channel_type`, `kind` | The chunk splitter closed and reopened a code fence across a message boundary. |
| `copperclaw_delivery_fence_unbalanced_input_total` | `channel_type` | The splitter saw a never-closed code fence in its input. |
| `copperclaw_markdown_render_total` | `flavor` | One render through the shared per-platform markdown renderer. |
| `copperclaw_markdown_unbalanced_marker_total` | `flavor` | The renderer hit the forgiving path for an unbalanced inline marker. |
| `copperclaw_inbound_files_total` | `channel`, `outcome` | Inbound attachment materialized (`ok\|too_large\|download_failed`). |
| `copperclaw_inbound_file_bytes` (histogram) | `channel` | Downloaded attachment size — tunes `max_attachment_bytes`. |
| `copperclaw_inbound_reaction_total` | `signal`, `outcome` | An inbound reaction reached the runner's mid-turn steering seam. |
| `copperclaw_slash_commands_total` | `command`, `channel_type` | A slash command (`stop\|status\|compact\|clear`) detected on an inbound. |
| `copperclaw_control_rows_written_total` | `command` | A mid-turn control row was written. |
| `copperclaw_status_answer_seconds` (histogram) | — | Wall-clock to synthesize a host-side `/status` reply. |
| `copperclaw_stuck_inbound_apology_total` | `agent_group_id`, `reason` | The sweep apologized to a user for a stuck inbound. |

#### Containers + images

| Name | Labels | Meaning |
|---|---|---|
| `copperclaw_containers_spawned_total` | — | Successful `runtime.spawn`. |
| `copperclaw_containers_crashed_total` | — | `CrashRestart` (heartbeat stale). |
| `copperclaw_container_spawn_seconds` (histogram) | — | Spawn duration. |
| `copperclaw_image_rebuild_total` | `image_profile`, `result` | Session-image rebuild by profile (`minimal\|prototyping`), `result` `ok\|failed`. |
| `copperclaw_image_rebuild_failed_total` | — | Rebuild error — the spawn falls back to the last-known-good tag. |
| `copperclaw_sessions_spawned_profile_total` | `profile` | One session spawn, attributed to its resolved tool profile. |
| `copperclaw_system_prompt_bytes` (histogram) | `profile` | Assembled system-prompt size at spawn. |
| `copperclaw_group_image_profile` (gauge) | `agent_group_id`, `image_profile` | Which groups run which image profile. |
| `copperclaw_image_bundle_version` (gauge) | `profile`, `pinned_binary`, `version` | Which pinned-binary version each image profile carries. |
| `copperclaw_pinned_binary_fetch_total` | `binary`, `outcome` | `fetch_pinned_binary` during an image build/rebuild. |

#### LLM calls, providers, compaction

| Name | Labels | Meaning |
|---|---|---|
| `copperclaw_llm_call_seconds` (histogram) | — | Duration of one LLM call. |
| `copperclaw_llm_tokens_input` / `_output` (histograms) | — | Token counts from `ProviderEvent::Usage`. |
| `copperclaw_provider_deadline_total` | `provider` | A provider call hit its deadline. |
| `copperclaw_provider_retry_total` | `provider` | A provider call was retried. |
| `copperclaw_provider_failover_total` | `from`, `to` | Mid-turn hot failover switched providers. |
| `copperclaw_provider_failover_chain_exhausted_total` | `provider` | The whole failover chain was exhausted; the turn hit the apology. |
| `copperclaw_compaction_triggered_total` | `profile` | Auto token-threshold compaction fired. |
| `copperclaw_compaction_estimated_tokens` (histogram) | — | Estimated history tokens at the compaction trigger. |
| `copperclaw_compaction_facts_header_bytes` (histogram) | — | Project facts header carried across a compaction. |
| `copperclaw_compaction_file_inventory_count` / `_bytes` (histograms) | — | Pinned project file inventory size. |
| `copperclaw_compaction_verify_stages_pinned` (histogram) | — | Verify stages pinned verbatim into the facts header. |
| `copperclaw_compaction_decisions_tail_lines` (histogram) | — | `DECISIONS.md` tail lines pinned into the facts header. |

#### Budgets + loop guards

| Name | Labels | Meaning |
|---|---|---|
| `copperclaw_budget_exhausted_total` | `agent_group_id`, `gate` | Spawn refused by a budget gate (`daily_tokens\|turns_per_minute\|turns_per_hour`). |
| `copperclaw_budget_exhausted_replies_total` | `agent_group_id` | Budget/rate-limit notice actually written to outbound (post-dedup). |
| `copperclaw_budget_exhausted_suppressed_total` | `agent_group_id` | Refusal notice suppressed by the per-group dedup window. |
| `copperclaw_task_budget_exhausted_total` | `agent_group_id` | The per-task token budget (`COPPERCLAW_MAX_TASK_TOKENS`) was hit. |
| `copperclaw_tool_loop_breaker_total` | `agent_group_id`, `pattern` | The runner broke a repeating tool-call loop. |

#### Task feedback (HUD, progressive reveal, approvals)

| Name | Labels | Meaning |
|---|---|---|
| `copperclaw_hud_posts_total` | `agent_group` | The HUD posted its first (or final-only) message for a turn. |
| `copperclaw_hud_edits_total` | `agent_group`, `trigger` | An in-place HUD edit fired (`batch_start\|batch_end\|ticker\|finalize`). |
| `copperclaw_hud_thinking_frame_total` | `agent_group` | The HUD posted its pre-first-tool "thinking" frame. |
| `copperclaw_hud_finalize_seconds` (histogram) | — | Turn duration at HUD finalize. |
| `copperclaw_hud_edit_total` | `channel_type`, `result` | Adapter-level HUD self-edit attempt (`ok\|error\|unsupported_fallthrough`). |
| `copperclaw_hud_degraded_total` | `channel_type`, `reason` | The HUD fell back to status rows instead of a live self-editing message. |
| `copperclaw_slack_hud_decision_total` | `typing_indicator_visible` | How the slack HUD typing-capability predicate resolved. |
| `copperclaw_slack_typing_skipped_total` / `_set_status_total` | `reason` / `result` | Slack typing-indicator declines and set-status outcomes. |
| `copperclaw_progressive_final_total` | `agent_group`, `outcome` | Final-answer reveal fired (`grown`) or fell through (`single_emit`). |
| `copperclaw_progressive_final_steps` / `_answer_chars` (histograms) | — | Edit steps and length per grown answer. |
| `copperclaw_progressive_final_skipped_total` | `reason` | Which gate arm declined growth. |
| `copperclaw_wall_card_total` | `blocker` | A curated "I'm blocked" wall card was emitted. |
| `copperclaw_blocked_todo_render_total` | `channel_type`, `has_reason` | A delivered todo checklist carried a `blocked` item. |
| `copperclaw_midturn_control_total` | `agent_group`, `kind` | A mid-turn `/stop` was honored or interjections were consumed. |
| `copperclaw_approval_card_outcome_total` | `outcome` | Approval-card lifecycle event. |
| `copperclaw_approval_taps_total` | `outcome` | In-chat approval tap resolution (`approved\|denied\|unauthorized\|race_noop`). |

#### Tool surface

| Name | Labels | Meaning |
|---|---|---|
| `copperclaw_shell_truncated_total` | `mode` | A shell stream was capped (`head\|tail`). |
| `copperclaw_shell_truncated_bytes` (histogram) | — | Pre-cap size of truncated shell streams. |
| `copperclaw_read_file_lines_mode_total` | — | A `read_file` call used lines mode. |
| `copperclaw_read_file_pages` (histogram) | — | Windowed reads needed to cover a whole file. |
| `copperclaw_load_skill_total` | `skill`, `mode` | A `load_skill` call (`inline\|callable`). |
| `copperclaw_skills_saved_total` | `outcome` | An approved `save_skill` write reached disk. |
| `copperclaw_memory_writes_total` | `provenance` | A `memory_save` committed (`trusted\|untrusted`). |
| `copperclaw_memory_write_rate_capped_total` | — | A `memory_save` refused by the per-session write ceiling. |
| `copperclaw_self_mod_succeeded_total` / `_failed_total` | `action` | Self-modification actions (`install_packages` / `add_mcp_server` / ...). |
| `copperclaw_unknown_tool_total` | `tool` | MCP dispatch had no entry for the requested tool name. |

#### Coding gates + session installs

| Name | Labels | Meaning |
|---|---|---|
| `copperclaw_verify_run_total` | `result` | A matched verify command ran (`pass\|fail`). |
| `copperclaw_verify_run_stage_total` | `stage`, `result` | Verify run attributed to its named stage. |
| `copperclaw_verify_stages_declared` (histogram) | — | Stages parsed from a project's `.copperclaw/verify`. |
| `copperclaw_verify_gate_completion_total` | `outcome` | A todo completion crossed the verify gate (`refused_dirty\|blocked_cycle_cap\|passed`). |
| `copperclaw_verify_gate_pending_stages_total` | `pending` | Completion refused by the multi-stage verify gate. |
| `copperclaw_verify_gate_fix_cycles` (histogram) | — | Fix cycles until a dirty marker cleared. |
| `copperclaw_review_gate_completion_total` | `outcome` | A delivery todo completion crossed the self-review gate. |
| `copperclaw_self_review_submission_total` | `kind` | A `self_review` submission (`no_findings\|findings`). |
| `copperclaw_self_review_findings` (histogram) | — | Findings per `self_review` call. |
| `copperclaw_diagnostics_run_total` | `tool`, `outcome` | A `diagnostics` linter/typechecker run (`eslint\|tsc\|ruff`). |
| `copperclaw_session_install_total` | `ecosystem`, `outcome` | A session-scope install (`pip\|npm`). |
| `copperclaw_session_install_seconds` (histogram) | — | Session-install wall-clock. |
| `copperclaw_session_install_egress_hint_total` | `ecosystem` | Install failed with an egress-denial hint surfaced to the model. |
| `copperclaw_session_install_image_scope_rejected_total` | — | A `scope:"image"` install carrying pip packages was rejected. |

#### Delegation

| Name | Labels | Meaning |
|---|---|---|
| `copperclaw_delegate_spawn_total` | `tier`, `outcome` | A `create_agent` / `delegate` spawn-gate outcome. |
| `copperclaw_delegate_depth_rejections_total` | `tier` | Spawn rejected by the nesting depth cap. |
| `copperclaw_delegate_worktree_provision_seconds` (histogram) | — | `git worktree add` latency for a delegate's writable worktree. |
| `copperclaw_delegate_batch_width` (histogram) | — | Workers fanned out per `delegate_batch` call. |
| `copperclaw_delegate_batch_worker_total` | `outcome` | One joined batch worker's terminal status. |
| `copperclaw_delegate_batch_refused_total` | — | A whole `delegate_batch` refused (no worker spawned). |
| `copperclaw_delegate_batch_contract_total` | `present` | Whether a batch call carried the shared `contract` arg. |
| `copperclaw_delegate_batch_post_join_dirty_total` | `outcome` | Post-join integration-verify dirty-mark decision. |

#### Browser + vision

| Name | Labels | Meaning |
|---|---|---|
| `copperclaw_browser_render_total` | `mode`, `outcome` | A `browser_render` call (`screenshot\|dom_text\|aria`). |
| `copperclaw_browser_render_duration_seconds` (histogram) | — | Navigate-to-artifact span. |
| `copperclaw_browser_render_screenshots_total` | `result` | Screenshot-mode renders. |
| `copperclaw_browser_screenshot_duration_seconds` (histogram) | — | Spawn-to-PNG-on-disk span. |
| `copperclaw_browser_child_spawn_total` / `_teardown_total` | `result` | Browser child-container lifecycle; teardown errors surface leaked children. |
| `copperclaw_browser_cdp_connect_failures_total` | — | CDP connect to the browser child failed. |
| `copperclaw_browser_ssrf_block_total` | `stage` | An SSRF guard refused a target (`target_preflight\|redirect_hop`). |
| `copperclaw_browser_render_preview_allow_injected_total` | — | The preview host:port egress-allow injection fired. |
| `copperclaw_browser_interactive_actions_total` | `action`, `outcome` | One scripted interactive-browser action. |
| `copperclaw_browser_output_format_total` | `tool`, `format` | Chosen (or downgraded-to) screenshot output format. |
| `copperclaw_chromium_singleton_spawn_total` | `result` | The in-container chromium singleton started a headless instance. |
| `copperclaw_chromium_singleton_idle_reap_total` | — | The singleton was torn down after idling past its reap threshold. |
| `copperclaw_ui_screenshot_total` | `outcome`, `viewport` | One `ui_screenshot` call. |
| `copperclaw_ui_screenshot_capture_seconds` (histogram) | — | Navigate/wait-to-bytes span per call. |
| `copperclaw_ui_screenshot_refused_url_total` | — | Refused because the `url` arg was not loopback. |
| `copperclaw_ui_inspect_total` | `outcome` | One `ui_inspect` call. |
| `copperclaw_ui_inspect_console_errors` (histogram) | — | Buffered console errors surfaced per `ui_inspect` call. |
| `copperclaw_see_fix_screenshots_per_build` (histogram) | — | `ui_screenshot` calls per inbound's tool loop (see→fix cycle proxy). |
| `copperclaw_ritual_screenshot_delivery_total` | `outcome` | A ritual screenshot was delivered to the user. |

#### Preview + tunnel

| Name | Labels | Meaning |
|---|---|---|
| `copperclaw_preview_expose_total` | `outcome` | A preview-expose call was `served` or hit `timeout`. |
| `copperclaw_preview_enable_card_total` | `outcome` | One-tap enable-preview approval-card lifecycle. |
| `copperclaw_preview_tombstoned` (gauge) | — | Currently-tombstoned previews. |
| `copperclaw_preview_tombstone_recovery_total` | `outcome` | Tombstoned-preview recovery attempts. |
| `copperclaw_preview_ws_upgrades_total` | `result` | WebSocket upgrades through the preview proxy (`ok\|refused\|upstream_502`). |
| `copperclaw_preview_ws_active` (gauge) | — | Currently-open preview WS bridges. |
| `copperclaw_preview_ws_frames_total` / `_bytes_total` | `direction` | Bridged frames / payload bytes. |
| `copperclaw_preview_ws_session_seconds` (histogram) | — | Bridge lifetime. |
| `copperclaw_public_tunnel_total` | `outcome`, `reason` | Public-tunnel (`make_preview_public`) lifecycle event. |

#### Channel adapter surfaces

| Name | Labels | Meaning |
|---|---|---|
| `copperclaw_adapter_surface_write_total` | `channel_type`, `mode` | A pinned rich surface (todo card / HUD anchor) was written. |
| `copperclaw_adapter_rich_render_total` | `channel_type`, `surface` | A channel rendered a native rich surface (`card\|diff\|todo\|...`). |
| `copperclaw_adapter_edit_message_total` | `channel_type`, `result` | Low-level adapter edit API call. |
| `copperclaw_adapter_reaction_total` | `channel_type`, `result` | Outbound emoji-reaction send (`ok\|unsupported\|error`). |
| `copperclaw_adapter_typing_total` | `channel_type`, `result` | Adapter typing-indicator send (`ok\|rate_limited\|unsupported\|error`). |
| `copperclaw_edit_drift_fallthrough_total` | `channel_type` | A HUD/approval edit hit the trait-default "not edit-capable" fallthrough. |
| `copperclaw_shared_renderer_adoption` (gauge) | `channel_type` | Which adapters use the shared markdown renderer. |

#### Scheduling

| Name | Labels | Meaning |
|---|---|---|
| `copperclaw_scheduled_task_fires_total` | `kind` | A durable scheduled task fired (`recurring_rearm` / one-shot). |
| `copperclaw_scheduled_task_fire_latency_seconds` (histogram) | — | Fire lateness vs `next_fire` (bounded by the sweep's 60s cadence). |
| `copperclaw_scheduled_tasks_active` (gauge) | — | Active rows in the central `tasks` table at the last sweep. |

#### Security + fleet health

| Name | Labels | Meaning |
|---|---|---|
| `copperclaw_policy_denied_total` | `layer`, `tool` | A tool call refused by a policy layer (`role\|skill\|profile\|provenance\|deny_list\|allow_list`). |
| `copperclaw_broker_requests_total` | `agent_group_id`, `outcome` | Credential-broker requests. |
| `copperclaw_broker_egress_bytes_total` | `agent_group_id` | Bytes proxied through the broker. |
| `copperclaw_secrets_rotated_total` | — | SIGHUP secret rotations. |
| `copperclaw_degraded_state` (gauge) | `reason` | 1 while a subsystem is flagged degraded, 0 when cleared. |

#### Stability + operator surfaces (M21)

Added by the M21 stability program (S/F/O cards); the metric definitions
and helper docs live in `crates/copperclaw-metrics/src/lib.rs`.

| Name | Labels | Meaning |
|---|---|---|
| `copperclaw_supervised_loop_alive` (gauge) | `loop` | 1 while a supervised background loop is running, 0 while drained or awaiting a backoff respawn (S1). |
| `copperclaw_supervised_loop_degraded` (gauge) | `loop` | 1 once a loop has exhausted its restart backoff curve, 0 once it heals (S1). |
| `copperclaw_supervised_loop_restarts_total` | `loop`, `reason` | A supervised loop was restarted after an unexpected exit (`reason` = `panicked\|returned`) (S1). |
| `copperclaw_container_restart_total` | `reason` | A session container was torn down for respawn: `crash` (dead heartbeat) or `stuck_tool` (wedged past the ceiling) (S2). |
| `copperclaw_container_oom_kills_total` | — | A session container was classified as OOM-killed at crash-restart time (S4). |
| `copperclaw_crash_backoff_level` (histogram) | — | The crash-loop streak position reached (how deep into the 5s→300s backoff curve) per recorded crash (S4). |
| `copperclaw_delivery_retry_resumed_total` | — | A pending outbound row's persisted retry counter resumed after a host restart instead of restarting from zero (S3). |
| `copperclaw_delivery_dead_letter_total` | `reason` | An outbound row was dead-lettered: `retry_exhausted` (S3) or `no_adapter` (channel had no live adapter past the age ceiling, S5). |
| `copperclaw_slow_spawn_notices_total` | — | The one-per-episode "setting things up" slow-spawn notice was posted to a user during a cold spawn (F1). The spawn-phase duration is `copperclaw_container_spawn_seconds`. |
| `copperclaw_question_expiries_total` | `outcome` | An `ask_user_question` TTL lapsed: `surfaced` (terminal note + no-answer result written) or `resolved_by_reply` (user answered after the ask; resolved silently) (F2). |
| `copperclaw_recovery_notices_total` | — | A boot-time "I was restarted mid-task" recovery notice was written for a session with an in-flight turn (F3). |
| `copperclaw_mcp_connection_cache_total` | `outcome` | External-MCP connection cache access: `hit`, `miss`, or `dead_retry` (cached connection found dead, evicted, retried once) (F4). |
| `copperclaw_mcp_connection_reaped_total` | — | Idle external-MCP connections closed by the reaper past their idle TTL (F4). |
| `copperclaw_integrity_quick_check_total` | `scope`, `outcome` | A SQLite `quick_check` probe ran; `scope` = `session\|central`, `outcome` = `healthy\|missing\|corrupt` (O2). |
| `copperclaw_integrity_quarantines_total` | — | A session's per-session DB failed `quick_check` and was quarantined + sweep-excluded (O2). |
| `copperclaw_integrity_quarantined_sessions` (gauge) | — | Current count of quarantined (sweep-excluded) sessions, observed each sweep pass (O2). |
| `copperclaw_provider_failover_transition_total` | `direction`, `from`, `to` | A live provider failover moved the serving provider: `direction` = `degrade` (to a fallback) or `restore` (back toward primary) (O3). |
| `copperclaw_provider_failover_active_position` (gauge) | — | Chain index (0 = primary) of the provider currently serving this session's turn (O3; one runner process = one session). |
| `copperclaw_operator_alerts_total` | `severity`, `outcome` | An operator-alert decision; `outcome` = `sent\|suppressed_disabled\|suppressed_deduped\|suppressed_rate_limited\|no_carrier\|enqueue_failed` (O4). |
| `copperclaw_sweep_last_run_timestamp` (gauge) | — | Unix time (seconds) of the last completed sweep pass — alert on `time() - <this>` to catch a wedged sweep loop. |

#### Codebase, autonomy & skills (M22)

Added by the M22 program (Wave 1 coding C1–C6, Wave 2 autonomy A1–A5, Wave 3
skills S1–S4); the metric definitions and helper docs live in
`crates/copperclaw-metrics/src/lib.rs`.

**Wave 1 — coding: prototype → codebase**

| Name | Labels | Meaning |
|---|---|---|
| `copperclaw_post_edit_verify_total` | `tool`, `outcome` | One post-edit verify over a just-mutated file; `tool` = `eslint\|tsc\|ruff\|none`, `outcome` = `flagged\|clean\|not_available\|unsupported\|disabled\|error` (C1). |
| `copperclaw_post_edit_verify_findings` (histogram) | — | Error+warning count for a post-edit verify that flagged (C1). |
| `copperclaw_repo_attach_total` | — | The runner attached an existing repo as the working project (once per genuine attach) (C2). |
| `copperclaw_repo_attach_verify_stages_inferred` (histogram) | — | Verify stages inferred from a repo's manifests at attach time (C2). |
| `copperclaw_repo_attach_detected_total` | — | The host's cold start noticed an attachable repo under a session's `/data` (host-side companion to the attach counter) (C2). |
| `copperclaw_find_symbol_total` | `definition_source` | One `find_symbol` lookup, labelled by the tier that resolved it: `ctags-index\|ctags-ondemand\|grep\|none` (C3). |
| `copperclaw_symbol_index_builds_total` | `backend` | A symbol-index build over an attached repo; `backend` = `language-server-assisted\|ctags\|none` (C3). |
| `copperclaw_symbol_index_symbols` (histogram) | — | Symbols written by an index build (0 when none built) (C3). |
| `copperclaw_visual_regression_flags_total` | `viewport`, `dimensions_changed` | A post-edit screenshot diff flagged a regression; `viewport` = `desktop\|mobile`, `dimensions_changed` = `true\|false` (C4). |
| `copperclaw_visual_regression_baselines_total` | — | A view's baseline PNG was (re)written after a capture (C4). |
| `copperclaw_review_batch_reviewers_total` | — | Reviewer workers dispatched in a `delegate_batch` call (incremented by the reviewer count) (C5). |
| `copperclaw_review_merge_gate_total` | `outcome` | The merge-gate verdict of a reviewer-bearing batch; `outcome` = `blocked\|passed` (C5). |
| `copperclaw_see_fix_gate_completion_total` | `outcome` | A final/delivery todo crossed the see→fix (post-fix screenshot) gate; `outcome` = `refused_needs_screenshot\|blocked_cycle_cap\|passed` (C6). |

**Wave 2 — autonomy: propose → act, safely**

| Name | Labels | Meaning |
|---|---|---|
| `copperclaw_task_grants_total` | `outcome` | A task capability-grant lifecycle event; `outcome` = `approved` (persisted after operator approval) or `revoked` (operator revoked via `cclaw grants revoke` / the `grants.revoke` wire command — M24 S3). `issued\|expired` remain reserved for when those sites land (there is no expiry sweep; grants lapse lazily via `effective_grant`) (A1). |
| `copperclaw_autonomous_actions_total` | `outcome` | An autonomous turn's credentialed external action met the grant gate; `outcome` = `taken` (granted, in-scope, fire charged) or `blocked_proposed` (ungranted/out-of-scope → read-then-propose) (A2). |
| `copperclaw_grants_snapshotted_total` | `outcome` | The host wrote/removed the per-session `grant.json` the runner's gate reads; `outcome` = `written\|removed_no_grant\|removed_no_firing_task\|removed_read_error` (A2 host half). |
| `copperclaw_grant_fires_consumed_total` | — | Grant fires the host debited after an autonomous action (A2 host half). |
| `copperclaw_grant_tokens_consumed_total` | — | Grant token budget the host debited after an autonomous action (A2 host half). |
| `copperclaw_goal_checkins_fired_total` | — | Goal check-in wakes the sweep synthesised (one per due active goal), consumed from the sweep report (A3). |
| `copperclaw_goals_budget_paused_total` | — | Goals the sweep paused because their grant-backed budget was exhausted (A3). |
| `copperclaw_goal_status_total` | `status` | A goal reached a terminal status via `update_goal`; `status` = `completed\|abandoned` (A3). |
| `copperclaw_goal_progress_recorded_total` | — | An `update_goal` call recorded a progress note (A3). |
| `copperclaw_active_goals` (gauge) | — | Count of `active` goals observed each sweep pass (A3). |
| `copperclaw_condition_checkins_fired_total` | `kind` | A stored condition fired a check-in wake on its rising edge; `kind` = `pending_inbound\|idle\|flag` (A4). |
| `copperclaw_recurrence_consolidated_total` | `outcome` | A per-session recurrence series was consolidated into the central `tasks` scheduler; `outcome` = `created\|already_present` (A5). |

**Wave 3 — skills: real capabilities**

| Name | Labels | Meaning |
|---|---|---|
| `copperclaw_skills_materialized_total` | `agent_group` | Skill dirs symlinked into a session container's `/data/skills` at cold start (incremented by the count reaching the container) (S1). |
| `copperclaw_skills_relevance_filtered_total` | — | Skills dropped from the inline prompt by a `Relevant` selector narrowing (registry total − selected), vs `All` (S2). |
| `copperclaw_skills_listed_total` | `mode` | A `list_skills` call answered; `mode` = `catalogue` (callable) or `inline_empty` (no catalogue on disk) (S3). |
| `copperclaw_skill_version_saved` (histogram) | — | The effective version persisted by an approval-gated `save_skill` (1 first save, N+1 on re-save) (S3). |
| `copperclaw_load_skill_inline_scoped_total` | `skill`, `scope` | An inline-mode `load_skill` resolved a skill's tool scope; `scope` = `narrowed` (declared a tool allowlist) or `cleared` (no scope) — sibling of `copperclaw_load_skill_total` (S4). |

### Recommended alerts

- `rate(copperclaw_containers_crashed_total[5m]) > 0` — runners dying;
  inspect host stderr / `COPPERCLAW_LOG_DIR` for the cause.
- `rate(copperclaw_image_rebuild_failed_total[1h]) > 0` — operator-
  supplied config (`packages_apt`, `packages_npm`) is broken; the
  group is still serving requests on a stale image. Fix the config or
  the agent loses future package contributions.
- `rate(copperclaw_delivery_failed_total[15m]) > 0` — a channel is
  dropping messages after 3 retries; check the channel's auth /
  rate-limit headers via the delivery logs.
- `sum by (agent_group_id, gate) (rate(copperclaw_budget_exhausted_total[15m])) > 0`
  — an agent group is repeatedly hitting a budget or rate-limit gate.
  Refusals come in three flavours via the `gate` label: `daily_tokens`
  (the daily-token cap), `turns_per_minute`, and `turns_per_hour`. The
  fix is operator-side: raise the cap with `cclaw budgets set --agent-group-id <id> --daily-tokens <n>` (also `--turns-per-minute` / `--turns-per-hour`) or
  investigate why the group is burning tokens / turns so fast. Pair
  with `copperclaw_budget_exhausted_replies_total` (notices that actually
  went to the user) and `copperclaw_budget_exhausted_suppressed_total`
  (notices the dedup window swallowed) to see the user-visible
  notification rate independent of refusal volume.
- `copperclaw_degraded_state > 0` — a subsystem has flagged itself
  degraded; the `reason` label says which. Check `cclaw doctor` and
  the host log.
- `rate(copperclaw_provider_failover_chain_exhausted_total[15m]) > 0`
  — every provider in a group's failover chain is unhealthy; turns
  are ending in the apology. Check provider keys / upstream status.
- `rate(copperclaw_stuck_inbound_apology_total[1h]) > 0` — users are
  receiving "I hit a snag" apologies; correlate with
  `copperclaw_containers_crashed_total` and the sweep logs.
- `rate(copperclaw_browser_ssrf_block_total[1h]) > 0` — something in
  a container asked the browser to fetch a blocked target. Occasional
  hits are the guard doing its job; a sustained rate deserves a look
  at the session transcripts.
- `rate(copperclaw_delivery_fence_unbalanced_input_total[1h])`,
  `rate(copperclaw_hud_degraded_total[1h])`, and
  `rate(copperclaw_edit_drift_fallthrough_total[1h])` — quality
  regressions in the outbound rendering path; not urgent, but a
  sustained rise usually means an adapter capability drifted.
- `min(copperclaw_supervised_loop_alive) == 0` or
  `max(copperclaw_supervised_loop_degraded) > 0` — a background loop is
  down or has exhausted its restart backoff (S1). Pair with
  `rate(copperclaw_supervised_loop_restarts_total[15m]) > 0` and check
  `cclaw doctor` / the host log for the panic reason.
- `time() - copperclaw_sweep_last_run_timestamp > 180` — the 60s sweep
  loop has not completed a pass in 3 minutes; stuck-session detection,
  recurrence fan-out, and GC are stalled. Complements the S1 liveness
  gauge for the sweep loop specifically.
- `rate(copperclaw_container_oom_kills_total[15m]) > 0` — sessions are
  being OOM-killed; raise `memory_mb` for the affected agent group.
  Watch `copperclaw_crash_backoff_level` skewing toward the cap for a
  crash-loop that backoff alone won't fix.
- `rate(copperclaw_container_restart_total{reason="stuck_tool"}[1h]) > 0`
  — tools are wedging past the absolute ceiling and the sweep is
  restarting their containers (S2); correlate with the session
  transcripts.
- `rate(copperclaw_delivery_dead_letter_total[1h]) > 0` — outbound rows
  are being dead-lettered. `reason="no_adapter"` means a channel has no
  live adapter (wire/fix it, then `cclaw dropped-messages replay`);
  `reason="retry_exhausted"` means an adapter kept failing.
- `rate(copperclaw_integrity_quarantines_total[1h]) > 0` or
  `copperclaw_integrity_quarantined_sessions > 0` — a per-session DB was
  found corrupt and quarantined (O2). A `corrupt` sample on the central
  scope of `copperclaw_integrity_quick_check_total{scope="central"}` is
  more serious — the central DB needs an operator restore from backup.
- `rate(copperclaw_provider_failover_transition_total{direction="degrade"}[15m]) > 0`
  — live failover is degrading off the primary provider (O3); pair with
  `copperclaw_provider_failover_chain_exhausted_total` and the
  `copperclaw_provider_failover_active_position` gauge to see whether the
  chain is holding.
- `sum by (severity) (rate(copperclaw_operator_alerts_total{outcome!="sent"}[1h])) > 0`
  — operator alerts are being suppressed rather than delivered (O4). A
  sustained `suppressed_disabled` means the alert destination is not
  configured (`COPPERCLAW_OPERATOR_ALERT_CHANNEL` / `_TARGET`);
  `no_carrier` / `enqueue_failed` mean the alert pipe couldn't place the
  outbound row.

### Scrape config

```yaml
# prometheus.yml
scrape_configs:
  - job_name: copperclaw
    static_configs:
      - targets: ["127.0.0.1:9090"]
```

The endpoint returns text/plain `# HELP` + `# TYPE` headers followed
by the per-metric samples, exactly as Prometheus expects.

## Log rotation

Without configuration, `tracing` writes to stderr. For a long-lived
daemon this grows the unit's journal or your `systemd` log target
unbounded. Set `COPPERCLAW_LOG_DIR` to fan tracing output to a
daily-rotating file as well, while keeping the stderr writer for unit
captures.

```
export COPPERCLAW_LOG_DIR=/var/log/copperclaw
```

The file naming convention is `<dir>/host.log.<YYYY-MM-DD>`. Files
are not auto-deleted — the daily rotation never shrinks the directory.
For retention, layer your platform's standard tool on top:

```
# /etc/logrotate.d/copperclaw — example, paired with COPPERCLAW_LOG_DIR=/var/log/copperclaw
/var/log/copperclaw/host.log.* {
    weekly
    rotate 8
    compress
    delaycompress
    missingok
    notifempty
}
```

`COPPERCLAW_LOG` (the existing `tracing-subscriber` env filter) still
controls the verbosity, e.g. `COPPERCLAW_LOG=info,copperclaw_host=debug`.
Filter changes apply to both the stderr writer and the rolling file
writer.

## What is **not** covered yet

- No distributed-tracing (OpenTelemetry) export. The data is in
  `agent_turns` + `audit_log` and can be queried directly.
- No structured per-request span IDs propagated across the host /
  container boundary. The session id is the closest correlation
  handle today.
- Log rotation file naming is fixed to `host.log.<date>`. The
  underlying `tracing-appender` crate supports hourly rotation;
  Copperclaw deliberately exposes only the daily knob to keep the
  surface small.
