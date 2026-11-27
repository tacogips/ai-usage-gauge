import { For, type Accessor, type JSX } from "solid-js";
import type { BudgetResponse, PieChartID, RateLimitsResponse } from "./api";
import chatgptTemplateIcon from "./assets/chatgpt-template.png";
import claudeTemplateIcon from "./assets/claude-template.png";
import { CollapsibleSection } from "./CollapsibleSection";
import { creditFraction, formatCredits, isCreditChart, normalizePieCharts, pieChartIds, pieFraction, rateLimitCreditsFor, rateLimitStatusLabel, rateLimitWindowFor } from "./rateLimits";

const labels: Record<PieChartID, string> = {
  budget: "Budget",
  claude5h: "Claude Code 5h",
  claudeWeekly: "Claude Code weekly",
  claudeCredit: "Claude Code credit",
  codex5h: "Codex 5h",
  codexWeekly: "Codex weekly",
  codexCredit: "Codex credit",
  gemini5h: "Gemini 5h",
  geminiWeekly: "Gemini weekly",
};

const shortLabels: Record<PieChartID, string> = {
  budget: "Budget",
  claude5h: "5h",
  claudeWeekly: "Weekly",
  claudeCredit: "Credit",
  codex5h: "5h",
  codexWeekly: "Weekly",
  codexCredit: "Credit",
  gemini5h: "5h",
  geminiWeekly: "Weekly",
};

function VendorIcon(props: { id: PieChartID }) {
  if (props.id === "budget") {
    return (
      <svg class="pie-toggle-icon" viewBox="0 0 16 16" aria-hidden="true">
        <circle cx="8" cy="8" r="6" fill="none" stroke="currentColor" stroke-width="1.6" />
        <path d="M8 2 A6 6 0 0 1 14 8 L8 8 Z" fill="currentColor" />
      </svg>
    );
  }
  if (props.id.startsWith("claude")) {
    return (
      <span
        class="pie-toggle-icon pie-toggle-icon-mask"
        aria-hidden="true"
        style={{
          "-webkit-mask-image": `url(${claudeTemplateIcon})`,
          "mask-image": `url(${claudeTemplateIcon})`,
        }}
      />
    );
  }
  if (props.id.startsWith("codex")) {
    return (
      <span
        class="pie-toggle-icon pie-toggle-icon-mask"
        aria-hidden="true"
        style={{
          "-webkit-mask-image": `url(${chatgptTemplateIcon})`,
          "mask-image": `url(${chatgptTemplateIcon})`,
        }}
      />
    );
  }
  return (
    <svg class="pie-toggle-icon" viewBox="0 0 36 36" aria-hidden="true">
      <path
        d="M18 35 C21 25 25 21 35 18 C25 15 21 11 18 1 C15 11 11 15 1 18 C11 21 15 25 18 35 Z"
        fill="currentColor"
      />
    </svg>
  );
}

export function PieChartPanel(props: { selection: Accessor<PieChartID[]>; onToggle: (id: PieChartID) => void; budget: Accessor<BudgetResponse | undefined>; response: Accessor<RateLimitsResponse | undefined>; children?: JSX.Element }) {
  const selection = () => normalizePieCharts(props.selection());
  return <CollapsibleSection id="subscription-usage" title="Subscription usage" label="subscription usage" class="panel pie-chart-panel"
    actions={<div class="pie-toggles toggle-group" role="group" aria-label="Choose usage charts"><For each={pieChartIds}>{(id) => {
      const pressed = () => selection().includes(id);
      return <button type="button" aria-pressed={pressed() ? "true" : "false"} aria-label={labels[id]} title={labels[id]} disabled={selection().length === 1 && pressed()} onClick={() => props.onToggle(id)}><VendorIcon id={id} /><span>{shortLabels[id]}</span></button>;
    }}</For></div>}>
    <div class="pie-list"><For each={selection()}>{(id) => {
      const data = () => rateLimitWindowFor(id, props.response());
      const credit = () => rateLimitCreditsFor(id, props.response()).credits;
      const fraction = () => id === "budget" ? props.budget()?.visualFraction : isCreditChart(id) ? creditFraction(credit()) : data().window == null ? undefined : pieFraction(data().window!);
      const value = () => Math.min(Math.max(fraction() ?? 0, 0), 1);
      const unavailable = () => rateLimitStatusLabel(data().provider, new Date()) || "Unavailable";
      const detail = () => id === "budget" ? (fraction() == null ? "No budget set" : `${Math.round(value() * 100)}% used`)
        : isCreditChart(id) ? (credit() == null ? unavailable() : formatCredits(credit()!))
        : data().window == null ? unavailable() : `${Math.round(value() * 100)}% used`;
      return <div class="pie-item"><svg viewBox="0 0 36 36" role="img" aria-label={`${labels[id]}: ${detail()}`}><circle class="pie-track" cx="18" cy="18" r="15.9"/><circle class="pie-fill" cx="18" cy="18" r="8" stroke-dasharray={`${value() * 50.265} 50.265`} transform="rotate(-90 18 18)"/></svg><strong>{labels[id]}</strong><span>{detail()}</span></div>;
    }}</For></div>
    {props.children}
  </CollapsibleSection>;
}
