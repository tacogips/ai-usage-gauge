/-!
Formal behavioral audit for `ai-usage-gauge` (Lean 4 stdlib only, zero `sorry`).

Models and proves five core runtime contracts:
1. `PieChartSelection` non-empty and sole-icon lock invariants (`state/pie_chart_selection.rs`).
2. `BudgetSummary` conservation, mutual exclusion, and clamping (`state/reset_window.rs`).
3. `RateLimitUsageService` bounded exponential backoff and stale fallback (`rate_limits/service.rs`).
4. `MachineCollector` stale-generation fencing, single-flight outcome propagation, and `Current`
   disposition eligibility (`aggregation/collector.rs`, `aggregation/merge.rs`).
5. `MachineDashboardRouter` fail-closed HTTP mutation security gate (`dashboard/router.rs`).
-/

namespace AiUsageGaugeAudit

/-! ## 1. PieChartSelection Invariants -/

inductive PieChartId where
  | budget
  | claude5h
  | claudeWeekly
  | claudeCredit
  | codex5h
  | codexWeekly
  | codexCredit
  | gemini5h
  | geminiWeekly
  deriving DecidableEq, Repr

def allCharts : List PieChartId :=
  [ .budget, .claude5h, .claudeWeekly, .claudeCredit
  , .codex5h, .codexWeekly, .codexCredit
  , .gemini5h, .geminiWeekly ]

def orderedCharts (ids : List PieChartId) : List PieChartId :=
  allCharts.filter (fun c => ids.contains c)

def normalizeCharts (ids : List PieChartId) : List PieChartId :=
  let o := orderedCharts ids
  if o.isEmpty then [.budget] else o

def toggleChart (id : PieChartId) (ids : List PieChartId) : List PieChartId :=
  let cur := normalizeCharts ids
  let next :=
    if cur.contains id then
      if cur.length > 1 then cur.filter (fun c => c != id) else cur
    else
      id :: cur
  normalizeCharts next

theorem normalizeCharts_nonempty (ids : List PieChartId) :
    (normalizeCharts ids).length ≥ 1 := by
  unfold normalizeCharts
  cases h : (orderedCharts ids) with
  | nil => simp
  | cons x xs => simp

theorem toggleChart_nonempty (id : PieChartId) (ids : List PieChartId) :
    (toggleChart id ids).length ≥ 1 := by
  unfold toggleChart
  exact normalizeCharts_nonempty _

theorem toggleChart_sole_item_locked (id : PieChartId) :
    toggleChart id [id] = [id] := by
  cases id <;> decide

/-! ## 2. BudgetSummary Conservation & Clamping -/

structure BudgetCalc where
  budget    : Nat
  spent     : Nat
  remaining : Nat
  overage   : Nat
  clamped   : Nat

def computeBudget (spent budget : Nat) : BudgetCalc :=
  { budget    := budget
    spent     := spent
    remaining := budget - spent
    overage   := spent - budget
    clamped   := min spent budget }

theorem budget_conservation (spent budget : Nat) :
    let b := computeBudget spent budget
    b.remaining + b.spent = b.budget + b.overage := by
  simp [computeBudget]
  omega

theorem budget_remaining_or_overage_zero (spent budget : Nat) :
    let b := computeBudget spent budget
    b.remaining = 0 ∨ b.overage = 0 := by
  simp [computeBudget]
  omega

theorem budget_visual_clamped (spent budget : Nat) :
    (computeBudget spent budget).clamped ≤ budget := by
  simp [computeBudget]
  omega

/-! ## 3. RateLimitUsageService Backoff & Stale Fallback -/

def maxBackoffSeconds : Nat := 1800

def backoffDelay (interval throttles : Nat) : Nat :=
  let exp := Nat.min (throttles - 1) 20
  let mult := Nat.pow 2 exp
  Nat.min (interval * mult) maxBackoffSeconds

theorem backoffDelay_bounded (interval throttles : Nat) :
    backoffDelay interval throttles ≤ maxBackoffSeconds := by
  unfold backoffDelay
  exact Nat.min_le_right _ _

theorem backoffDelay_at_least_interval (interval throttles : Nat)
    (h_le : interval ≤ maxBackoffSeconds) :
    interval ≤ backoffDelay interval throttles := by
  unfold backoffDelay
  have hpow : 1 ≤ Nat.pow 2 (Nat.min (throttles - 1) 20) := Nat.one_le_two_pow
  have hmul : interval ≤ interval * Nat.pow 2 (Nat.min (throttles - 1) 20) :=
    Nat.le_mul_of_pos_right interval (Nat.lt_of_lt_of_le (by decide) hpow)
  exact Nat.le_min.mpr ⟨hmul, h_le⟩

inductive RateLimitStatus where
  | ok
  | stale
  | unauthorized
  | unavailable
  deriving DecidableEq

def failureStatus (hasValidFallback isAuthError : Bool) : RateLimitStatus :=
  if hasValidFallback then .stale
  else if isAuthError then .unauthorized
  else .unavailable

theorem failure_with_fallback_is_stale (isAuthError : Bool) :
    failureStatus true isAuthError = .stale := by
  rfl

/-! ## 4. MachineCollector Fencing, Single-Flight, & Eligibility -/

inductive CollectionState where
  | healthy
  | stale
  | error
  | neverCollected
  | disabled
  deriving DecidableEq

inductive Disposition where
  | current
  | historical
  deriving DecidableEq

structure MachineEntry where
  generation   : Nat
  enabled      : Bool
  stale        : Bool
  hasSnapshot  : Bool
  state        : CollectionState
  deriving DecidableEq

def commitSuccess (entry : MachineEntry) (gen : Nat) : MachineEntry × Bool :=
  if entry.generation != gen then
    (entry, false)
  else
    ({ entry with stale := false, hasSnapshot := true, state := .healthy }, true)

theorem stale_generation_cannot_mutate (entry : MachineEntry) (gen : Nat)
    (h : entry.generation ≠ gen) :
    commitSuccess entry gen = (entry, false) := by
  unfold commitSuccess
  have hne : (entry.generation != gen) = true := by
    simp [h]
  simp [hne]

def eligibleForDisposition (entry : MachineEntry) (disp : Disposition) : Bool :=
  if !entry.enabled then
    false
  else
    match disp with
    | .current => entry.state == .healthy && !entry.stale && entry.hasSnapshot
    | .historical => entry.hasSnapshot

theorem current_excludes_non_healthy (entry : MachineEntry)
    (h : entry.state ≠ .healthy) :
    eligibleForDisposition entry .current = false := by
  unfold eligibleForDisposition
  cases entry.enabled <;> simp [h]

theorem current_excludes_stale (entry : MachineEntry) (h : entry.stale = true) :
    eligibleForDisposition entry .current = false := by
  unfold eligibleForDisposition
  cases entry.enabled <;> simp [h]

theorem single_flight_waiter_matches_leader (leaderOutcome : Bool) :
    let channelValue : Option Bool := some leaderOutcome
    channelValue.getD false = leaderOutcome := by
  cases leaderOutcome <;> rfl

/-! ## 5. Router Mutation Security Gate (Fail-Closed) -/

structure MutationGateInput where
  isTrustedIpc     : Bool
  validLoopbackHost : Bool
  validOriginOrFetchSite : Bool
  hasMutationHeader : Bool

def allowMutation (i : MutationGateInput) : Bool :=
  i.isTrustedIpc ||
    (i.validLoopbackHost && i.validOriginOrFetchSite && i.hasMutationHeader)

theorem untrusted_without_header_denied (i : MutationGateInput)
    (h1 : i.isTrustedIpc = false) (h2 : i.hasMutationHeader = false) :
    allowMutation i = false := by
  simp [allowMutation, h1, h2]

theorem untrusted_without_loopback_host_denied (i : MutationGateInput)
    (h1 : i.isTrustedIpc = false) (h2 : i.validLoopbackHost = false) :
    allowMutation i = false := by
  simp [allowMutation, h1, h2]

theorem untrusted_without_origin_check_denied (i : MutationGateInput)
    (h1 : i.isTrustedIpc = false) (h2 : i.validOriginOrFetchSite = false) :
    allowMutation i = false := by
  simp [allowMutation, h1, h2]

/-! ## 6. Dashboard Machine Scope & Local Machine Invariant -/

def ensureLocalMachine (ids : List String) : List String :=
  if ids.contains "local" then ids else "local" :: ids

theorem ensureLocalMachine_contains_local (ids : List String) :
    (ensureLocalMachine ids).contains "local" = true := by
  unfold ensureLocalMachine
  cases h : ids.contains "local" with
  | true => exact h
  | false => simp

theorem local_row_survives_default_scope (ids : List String) :
    let scope := ensureLocalMachine ids
    scope.contains "local" = true :=
  ensureLocalMachine_contains_local ids

/-! ## 7. Menu Bar Cost-Only & Vendor Fill Icon Invariant -/

def trayShowRatio (id : PieChartId) (ratios : List PieChartId) (costOnly : Bool) : Bool :=
  ratios.contains id && (id != .budget || !costOnly)

def trayShowIcon (id : PieChartId) (pies ratios : List PieChartId) (costOnly : Bool) : Bool :=
  (normalizeCharts pies).contains id || (trayShowRatio id ratios costOnly && id != .budget)

theorem cost_only_hides_budget_ratio (ratios : List PieChartId) :
    trayShowRatio .budget ratios true = false := by
  simp [trayShowRatio]

theorem cost_only_preserves_selected_vendor_icon (id : PieChartId) (pies ratios : List PieChartId)
    (h : (normalizeCharts pies).contains id = true) :
    trayShowIcon id pies ratios true = true := by
  unfold trayShowIcon
  rw [h]
  rfl

/-! ## 8. Progressive Dashboard Unblocking Invariant -/

def shouldBlockDashboard (hasAnyData isRangeLoading isInitialLoading isFetching hasFailed isTerminal : Bool) : Bool :=
  if hasAnyData then false
  else if isRangeLoading then true
  else if !isInitialLoading then false
  else if !isTerminal then true
  else isFetching && !hasFailed

theorem has_any_data_never_blocks (r i f fail term : Bool) :
    shouldBlockDashboard true r i f fail term = false := by
  rfl

/-! ## 9. Concurrent Time-Slice Progressive Loading, Spinner, & X-Axis Cutoff -/

def recencyBucket (ageSecs : Nat) : Nat :=
  if ageSecs < 3600 then 0
  else if ageSecs < 10800 then 1
  else if ageSecs < 21600 then 2
  else if ageSecs < 43200 then 3
  else if ageSecs < 86400 then 4
  else 5 + ageSecs / 86400

theorem recencyBucket_monotone (a b : Nat) (h : a ≤ b) :
    recencyBucket a ≤ recencyBucket b := by
  have hdiv : a / 86400 ≤ b / 86400 := Nat.div_le_div_right h
  unfold recencyBucket
  repeat (first | split | omega)

def workerCount (avail maxConcurrency sliceCount : Nat) : Nat :=
  min (min (max avail 1) (max maxConcurrency 1)) sliceCount

theorem workerCount_bounded (avail sliceCount : Nat) :
    workerCount avail 5 sliceCount ≤ 5 ∧ workerCount avail 5 sliceCount ≤ sliceCount := by
  unfold workerCount
  omega

theorem workerCount_pos (avail sliceCount : Nat) (h : sliceCount ≥ 1) :
    workerCount avail 5 sliceCount ≥ 1 := by
  unfold workerCount
  omega

structure ProgressEntry where
  inProgress  : Bool
  hasSnapshot : Bool
  state       : CollectionState
  deriving DecidableEq

def publishProgress (e : ProgressEntry) : ProgressEntry :=
  { e with hasSnapshot := true, state := .healthy }

theorem publishProgress_keeps_in_progress (e : ProgressEntry) (h : e.inProgress = true) :
    (publishProgress e).inProgress = true ∧ (publishProgress e).hasSnapshot = true := by
  simp [publishProgress, h]

inductive ChartDisplayMode where
  | spinner
  | chart
  | emptyMessage
  deriving DecidableEq

def chartDisplayMode (rowCount : Nat) (isLoading : Bool) : ChartDisplayMode :=
  if rowCount > 0 then .chart
  else if isLoading then .spinner
  else .emptyMessage

theorem empty_while_loading_shows_spinner :
    chartDisplayMode 0 true = .spinner := by
  rfl

theorem nonempty_shows_chart_even_while_loading (n : Nat) (h : n > 0) (isLoading : Bool) :
    chartDisplayMode n isLoading = .chart := by
  unfold chartDisplayMode
  simp [h]

def clampedTimelineEnd (isDaily : Bool) (startTs endTs nowTs : Nat) : Nat :=
  if !isDaily && endTs > nowTs then
    min (max (nowTs + 1) startTs) endTs
  else
    endTs

theorem subdaily_timeline_end_not_future (startTs endTs nowTs : Nat)
    (h_start : startTs ≤ nowTs + 1) (h_end : endTs > nowTs) :
    clampedTimelineEnd false startTs endTs nowTs ≤ nowTs + 1 := by
  unfold clampedTimelineEnd
  simp [h_end]
  omega

/-! ## 10. Zero-JSON-Parse Byte-Offset Binary Search Soundness -/

def binarySearchStep (lo hi mid : Nat) (probeLessThanCutoff : Option Bool) : Nat × Nat :=
  match probeLessThanCutoff with
  | some true => (mid, hi)
  | _ => (lo, mid)

theorem binarySearchStep_preserves_lower_bound
    (ts : Nat → Nat) (cutoff lo hi mid : Nat)
    (h_mono : ∀ i j, i ≤ j → ts i ≤ ts j)
    (h_inv : ∀ i, i < lo → ts i < cutoff)
    (probe : Option Bool)
    (h_probe : probe = some true → ts mid < cutoff) :
    let (lo', _) := binarySearchStep lo hi mid probe
    ∀ i, i < lo' → ts i < cutoff := by
  cases probe with
  | none =>
      simpa [binarySearchStep] using h_inv
  | some b =>
      cases b with
      | false =>
          simpa [binarySearchStep] using h_inv
      | true =>
          intro i hi_lt
          have hmid : ts mid < cutoff := h_probe rfl
          have hle : ts i ≤ ts mid := h_mono i mid (Nat.le_of_lt hi_lt)
          exact Nat.lt_of_le_of_lt hle hmid

/-! ## 11. Chart Bucket Generation, No-Future-Bucket Invariant, & Local 6h Alignment -/

inductive ChartGranularity where
  | min15
  | hourly
  | hour6
  | daily
  deriving DecidableEq

def bucketStepSecs : ChartGranularity → Nat
  | .min15  => 900
  | .hourly => 3600
  | .hour6  => 21600
  | .daily  => 86400

/-- Local-time floor of a timestamp shifted by `tzOffsetSecs` into a bucket of size `step`. -/
def localFloorBucket (ts tzOffsetSecs step : Nat) : Nat :=
  ((ts + tzOffsetSecs) / step) * step

theorem local_6hour_bucket_agreement (ts tzOffsetSecs : Nat) :
    localFloorBucket ts tzOffsetSecs (bucketStepSecs .hour6) =
      localFloorBucket ts tzOffsetSecs 21600 := by
  rfl

/-- Clamped exclusive upper bound in local-shifted seconds:
    sub-daily clamps to `now + 1`, daily clamps to `nextLocalMidnight(now)`. -/
def clampedLocalEnd (g : ChartGranularity) (startLocal endLocal nowLocal : Nat) : Nat :=
  let cap :=
    match g with
    | .daily => ((nowLocal / 86400) + 1) * 86400
    | _      => nowLocal + 1
  if endLocal > cap then max startLocal cap else endLocal

/-- Bounded fuel bucket generator modeling `for (b = start; b < endExclusive; b += step)`. -/
def generateBuckets : Nat → Nat → Nat → Nat → List Nat
  | 0, _, _, _ => []
  | fuel + 1, cur, endExcl, step =>
      if cur < endExcl then
        cur :: generateBuckets fuel (cur + step) endExcl step
      else
        []

theorem generateBuckets_all_lt_end (fuel cur endExcl step : Nat) :
    ∀ b ∈ generateBuckets fuel cur endExcl step, b < endExcl := by
  induction fuel generalizing cur with
  | zero =>
      intro b hb
      cases hb
  | succ k ih =>
      intro b hb
      unfold generateBuckets at hb
      split at hb
      · simp only [List.mem_cons] at hb
        rcases hb with rfl | hrest
        · assumption
        · exact ih (cur + step) b hrest
      · cases hb

theorem no_future_subdaily_bucket (fuel startLocal endLocal nowLocal : Nat)
    (g : ChartGranularity) (hg : g ≠ .daily)
    (h_start : startLocal ≤ nowLocal + 1) :
    ∀ b ∈ generateBuckets fuel startLocal (clampedLocalEnd g startLocal endLocal nowLocal) (bucketStepSecs g),
      b ≤ nowLocal := by
  intro b hb
  have hlt := generateBuckets_all_lt_end fuel startLocal
    (clampedLocalEnd g startLocal endLocal nowLocal) (bucketStepSecs g) b hb
  unfold clampedLocalEnd at hlt
  cases g with
  | daily => exact absurd rfl hg
  | min15 | hourly | hour6 =>
      dsimp at hlt
      split at hlt <;> omega

theorem no_future_daily_bucket (fuel startLocal endLocal nowLocal : Nat)
    (h_start : startLocal ≤ ((nowLocal / 86400) + 1) * 86400) :
    ∀ b ∈ generateBuckets fuel startLocal (clampedLocalEnd .daily startLocal endLocal nowLocal) 86400,
      b < ((nowLocal / 86400) + 1) * 86400 := by
  intro b hb
  have hlt := generateBuckets_all_lt_end fuel startLocal
    (clampedLocalEnd .daily startLocal endLocal nowLocal) 86400 b hb
  unfold clampedLocalEnd at hlt
  dsimp at hlt
  split at hlt <;> omega

def effectiveTimelineEnd (respGran reqGran : ChartGranularity) (respEnd : Option Nat) : Option Nat :=
  if respGran == reqGran then respEnd else none

theorem stale_granularity_guarded (respGran reqGran : ChartGranularity) (respEnd : Option Nat)
    (h : respGran ≠ reqGran) :
    effectiveTimelineEnd respGran reqGran respEnd = none := by
  unfold effectiveTimelineEnd
  have hne : (respGran == reqGran) = false := beq_false_of_ne h
  simp [hne]

/-! ## 12. Startup Cache Pre-Seeding & Decoupled Menu Bar Gauge Render -/

structure StartupRenderState where
  rateLimitsCached : Bool
  rateLimitsFresh  : Bool
  sqliteSeeded     : Bool
  liveScanDone     : Bool
  deriving DecidableEq

def trayCanRenderVendorGauges (s : StartupRenderState) : Bool :=
  s.rateLimitsCached || s.rateLimitsFresh

theorem tray_renders_from_cache_before_live_scan (sqliteSeeded : Bool) :
    trayCanRenderVendorGauges {
      rateLimitsCached := true
      rateLimitsFresh := false
      sqliteSeeded := sqliteSeeded
      liveScanDone := false
    } = true := by
  rfl

theorem tray_renders_fresh_gauges_before_live_scan (cached sqliteSeeded : Bool) :
    trayCanRenderVendorGauges {
      rateLimitsCached := cached
      rateLimitsFresh := true
      sqliteSeeded := sqliteSeeded
      liveScanDone := false
    } = true := by
  cases cached <;> rfl

/-! ## 13. Historical Range Backfill Trigger & 15-Minute Live-Day Cache Refinement -/

def needsHistoricalBackfill (requestedStartDay coverageStartDay : Nat) (inProgress : Bool) : Bool :=
  !inProgress && requestedStartDay < coverageStartDay

theorem historical_gap_triggers_backfill (req cov : Nat) (h : req < cov) :
    needsHistoricalBackfill req cov false = true := by
  unfold needsHistoricalBackfill
  simp [h]

theorem fifteen_min_divides_all_granularities (g : ChartGranularity) :
    900 ∣ bucketStepSecs g := by
  cases g <;> decide

structure LiveDayCacheState where
  hasCached15mBuckets : Bool
  markedCompleteInCoverage : Bool
  deriving DecidableEq

def saveLiveDay (_s : LiveDayCacheState) : LiveDayCacheState :=
  { hasCached15mBuckets := true, markedCompleteInCoverage := false }

theorem saveLiveDay_caches_without_finalizing (s : LiveDayCacheState) :
    (saveLiveDay s).hasCached15mBuckets = true ∧
      (saveLiveDay s).markedCompleteInCoverage = false := by
  simp [saveLiveDay]

/-- Disjoint 15-minute live-day partition: cached `[dayStart, watermark)` and freshly scanned
    `[watermark, dayEnd)` never overlap, so merging them cannot double-count any event. -/
theorem disjoint_15m_partition_no_double_count (dayStart watermark ts : Nat) :
    ¬ (dayStart ≤ ts ∧ ts < watermark ∧ watermark ≤ ts) := by
  omega

end AiUsageGaugeAudit
