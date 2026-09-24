import React, { useMemo, useState } from 'react';
import {
  BarChart3,
  TrendingUp,
  DollarSign,
  Database,
  BrainCircuit,
  Download,
  Trash2,
  FileText,
  FolderOpen,
  CalendarRange,
} from 'lucide-react';
import { VirtualKey, ModelConfig, RequestLog } from '../../types';
import { WobblyCard, SketchBadge, SketchButton } from '../HandDrawnElements';
import { formatCurrency, formatTokens } from '../../lib/designSystem';
import { useConfirm } from '../../lib/useConfirm';
import type { ExportFile, UsageDay } from '../../lib/resources';

type Range = 'today' | '24h' | '7d' | '30d';

const RANGE_LABELS: { id: Range; label: string; ms: number | 'today' }[] = [
  { id: 'today', label: 'Today', ms: 'today' },
  { id: '24h', label: '24h', ms: 24 * 3600 * 1000 },
  { id: '7d', label: '7d', ms: 7 * 24 * 3600 * 1000 },
  { id: '30d', label: '30d', ms: 30 * 24 * 3600 * 1000 },
];

function inRange(ts: string, range: Range): boolean {
  const t = Date.parse(ts);
  if (Number.isNaN(t)) return true;
  const now = Date.now();
  if (range === 'today') {
    const d = new Date();
    d.setUTCHours(0, 0, 0, 0);
    return t >= d.getTime();
  }
  const ms = RANGE_LABELS.find((r) => r.id === range)?.ms;
  return typeof ms === 'number' ? t >= now - ms : true;
}

interface UsageViewProps {
  keys: VirtualKey[];
  models: ModelConfig[];
  requests: RequestLog[];
  exportFiles?: ExportFile[];
  exportDir?: string;
  exportRetentionDays?: number;
  exportDays?: UsageDay[];
  onExportDay?: (day?: string) => Promise<void>;
  onDeleteExport?: (name: string) => Promise<void>;
  onRefreshExports?: () => Promise<void> | void;
}

const formatPrice = (value: number | null, digits: number): string =>
  value == null ? '—' : `${value.toFixed(digits)}`;

export const UsageView: React.FC<UsageViewProps> = ({
  keys,
  models,
  requests,
  exportFiles = [],
  exportDir,
  exportRetentionDays,
  exportDays = [],
  onExportDay,
  onDeleteExport,
  onRefreshExports,
}) => {
  const [range, setRange] = useState<Range>('30d');
  const [busy, setBusy] = useState(false);
  const { confirm, confirmNode } = useConfirm();

  const filtered = useMemo(() => requests.filter((r) => inRange(r.timestamp, range)), [requests, range]);

  const totalSpend = keys.reduce((acc, k) => acc + k.currentMonthlySpend, 0);
  const totalTokens = keys.reduce((acc, k) => acc + k.totalTokens, 0);
  const totalCachedTokens = filtered.reduce((acc, r) => acc + r.cachedTokens, 0);
  const totalCacheWriteTokens = filtered.reduce((acc, r) => acc + r.cacheWriteTokens, 0);
  const totalThinkingTokens = filtered.reduce((acc, r) => acc + r.thinkingTokens, 0);

  const yesterday = useMemo(() => {
    const d = new Date();
    d.setUTCDate(d.getUTCDate() - 1);
    return d.toISOString().slice(0, 10);
  }, []);

  const handleExport = async (day?: string) => {
    if (!onExportDay) return;
    const label = day ?? yesterday;
    const ok = await confirm({
      title: `Export ${label} usage?`,
      message: `Write ${label}'s request logs to JSONL and CSV in the export directory.`,
      confirmLabel: 'Export',
    });
    if (!ok) return;
    setBusy(true);
    try {
      await onExportDay(day);
    } finally {
      setBusy(false);
    }
  };

  const handleDelete = async (name: string) => {
    if (!onDeleteExport) return;
    const ok = await confirm({
      title: 'Delete this export file?',
      message: `Permanently remove ${name} from disk.`,
      confirmLabel: 'Delete',
      danger: true,
    });
    if (!ok) return;
    setBusy(true);
    try {
      await onDeleteExport(name);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="space-y-6">
      {confirmNode}
      {/* Top Banner */}
      <div className="flex flex-col lg:flex-row lg:items-end justify-between gap-4">
        <div>
          <h2 className="text-3xl font-heading font-bold text-[var(--ink)] flex items-center gap-2">
            <span>Usage, Attribution & Spend</span>
            <SketchBadge variant="green" rotation="1deg">
              Accurate per FR-6
            </SketchBadge>
          </h2>
          <p className="text-base font-body text-[var(--ink)]/80">
            Every request is attributed to a team member or project, tracking inclusive input/output totals plus cache-read, cache-write, and reasoning breakdowns.
          </p>
        </div>

        {/* Storage window selector */}
        <div className="flex items-center gap-2 shrink-0">
          <CalendarRange className="w-5 h-5 text-[var(--ink)]/70" />
          <div className="flex border-2 border-[var(--ink)] overflow-hidden sketch-shadow-sm bg-[var(--surface)]">
            {RANGE_LABELS.map((r) => (
              <button
                key={r.id}
                onClick={() => setRange(r.id)}
                className={`px-3 py-1.5 text-sm font-heading font-bold cursor-pointer transition-colors border-r-2 border-[var(--ink)] last:border-r-0 ${
                  range === r.id ? 'bg-[var(--pen-blue)] text-[var(--surface)]' : 'bg-[var(--surface)] hover:bg-[var(--erased)]'
                }`}
              >
                {r.label}
              </button>
            ))}
          </div>
        </div>
      </div>

      {/* Top metric overview cards */}
      <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-4 gap-4">
        <WobblyCard decoration="tack" className="p-4 bg-[var(--postit)]">
          <div className="flex items-center justify-between">
            <span className="text-sm font-heading font-bold text-[var(--ink)]">Total Spend (MTD)</span>
            <DollarSign className="w-5 h-5 text-[var(--pen-blue)]" />
          </div>
          <div className="text-3xl font-heading font-bold text-[var(--ink)] mt-2">
            {formatCurrency(totalSpend)}
          </div>
          <span className="text-xs font-mono text-[var(--ink)]/70 block mt-1">
            Across {keys.length} active virtual keys
          </span>
        </WobblyCard>

        <WobblyCard decoration="tape" className="p-4 bg-[var(--surface)]">
          <div className="flex items-center justify-between">
            <span className="text-sm font-heading font-bold text-[var(--ink)]">Tokens Processed</span>
            <TrendingUp className="w-5 h-5 text-[var(--marker-red)]" />
          </div>
          <div className="text-3xl font-heading font-bold text-[var(--ink)] mt-2">
            {formatTokens(totalTokens)}
          </div>
          <span className="text-xs font-mono text-[var(--ink)]/70 block mt-1">
            Combined input &amp; output
          </span>
        </WobblyCard>

        <WobblyCard className="p-4 bg-[var(--surface)]">
          <div className="flex items-center justify-between">
            <span className="text-sm font-heading font-bold text-[var(--ink)]">Context Cache Savings</span>
            <Database className="w-5 h-5 text-[var(--pen-green)]" />
          </div>
          <div className="text-3xl font-heading font-bold text-[var(--pen-green)] mt-2">
            {formatTokens(totalCachedTokens)}
          </div>
          <span className="text-xs font-mono text-[var(--ink)]/70 block mt-1">
            Reads: {formatTokens(totalCachedTokens)} · writes: {formatTokens(totalCacheWriteTokens)} ({filtered.length} reqs)
          </span>
        </WobblyCard>

        <WobblyCard decoration="tack-blue" className="p-4 bg-[var(--surface)]">
          <div className="flex items-center justify-between">
            <span className="text-sm font-heading font-bold text-[var(--ink)]">Reasoning (Thinking)</span>
            <BrainCircuit className="w-5 h-5 text-[var(--pen-blue)]" />
          </div>
          <div className="text-3xl font-heading font-bold text-[var(--pen-blue)] mt-2">
            {formatTokens(totalThinkingTokens)}
          </div>
          <span className="text-xs font-mono text-[var(--ink)]/70 block mt-1">
            Reasoning tokens in window
          </span>
        </WobblyCard>
      </div>

      {/* Spend by Virtual Key & Budget Limits */}
      <WobblyCard decoration="tape" className="p-6">
        <h3 className="text-2xl font-heading font-bold text-[var(--ink)] mb-4 flex items-center gap-2">
          <BarChart3 className="w-6 h-6 text-[var(--marker-red)]" />
          Spend by Virtual Key vs. Configured Budgets
        </h3>

        <div className="space-y-4">
          {keys.map((k) => {
            const pct = k.monthlyBudget > 0 ? Math.min(100, Math.round((k.currentMonthlySpend / k.monthlyBudget) * 100)) : 0;
            const isNearCap = pct >= 80;

            return (
              <div
                key={k.id}
                className="p-4 bg-[var(--paper)] border-2 border-[var(--ink)] sketch-shadow-sm rounded-lg"
              >
                <div className="flex flex-col sm:flex-row items-start sm:items-center justify-between gap-2 mb-2">
                  <div>
                    <span className="font-heading font-bold text-lg text-[var(--ink)]">{k.name}</span>
                    <span className="text-xs font-mono text-[var(--ink)]/60 ml-2">
                      ({k.owner} • {k.tag})
                    </span>
                  </div>

                  <div className="flex items-center gap-2">
                    <span className="font-heading font-bold text-base">
                      {formatCurrency(k.currentMonthlySpend)} / {formatCurrency(k.monthlyBudget)}
                    </span>
                    {isNearCap && (
                      <SketchBadge variant="red" rotation="-1deg">
                        Cap Alert ({pct}%)
                      </SketchBadge>
                    )}
                  </div>
                </div>

                {/* Hand-drawn progress bar */}
                <div className="w-full h-4 bg-[var(--erased)] border-2 border-[var(--ink)] rounded-full overflow-hidden relative">
                  <div
                    className={`h-full border-r-2 border-[var(--ink)] transition-all duration-300 ${
                      pct > 80 ? 'bg-[var(--marker-red)]' : 'bg-[var(--pen-blue)]'
                    }`}
                    style={{ width: `${pct}%` }}
                  />
                </div>

                <div className="flex items-center justify-between text-xs font-mono text-[var(--ink)]/70 mt-2">
                  <span>Daily spend: {formatCurrency(k.currentDailySpend)}</span>
                  <span>Requests: {k.totalRequests.toLocaleString()}</span>
                  <span>Total tokens: {formatTokens(k.totalTokens)}</span>
                </div>
              </div>
            );
          })}
        </div>
      </WobblyCard>

      {/* Usage exports to disk */}
      <WobblyCard decoration="tack-blue" className="p-6">
        <div className="flex flex-col sm:flex-row sm:items-center justify-between gap-3 mb-3">
          <h3 className="text-2xl font-heading font-bold text-[var(--ink)] flex items-center gap-2">
            <Download className="w-6 h-6 text-[var(--pen-green)]" />
            Usage Exports (JSONL + CSV)
          </h3>
          <div className="flex items-center gap-2">
            {onRefreshExports && (
              <SketchButton variant="ghost" size="sm" onClick={() => onRefreshExports()} disabled={busy}>
                Refresh
              </SketchButton>
            )}
            {onExportDay && (
              <SketchButton variant="secondary" size="sm" onClick={() => handleExport()} disabled={busy} className="gap-1.5">
                <FileText className="w-4 h-4" /> Export {yesterday}
              </SketchButton>
            )}
          </div>
        </div>

        <p className="text-sm font-body text-[var(--ink)]/80 mb-3">
          Each closed day is written to <span className="font-mono">usage-&lt;day&gt;.jsonl</span> (one request per line),
          <span className="font-mono"> usage-&lt;day&gt;.csv</span>, and <span className="font-mono">summary-&lt;day&gt;.csv</span>.
          Files older than {exportRetentionDays ?? 30} days are pruned automatically; you can delete any file here or on disk.
        </p>

        {exportDir && (
          <p className="text-xs font-mono text-[var(--ink)]/60 mb-3 flex items-center gap-1.5 break-all">
            <FolderOpen className="w-3.5 h-3.5 shrink-0" /> {exportDir}
          </p>
        )}

        {exportDays.length > 0 && (
          <div className="flex flex-wrap gap-2 mb-4">
            {exportDays.slice(0, 14).map((d) => (
              <button
                key={d.day}
                onClick={() => handleExport(d.day)}
                disabled={busy || !onExportDay}
                className="px-2.5 py-1 bg-[var(--surface)] border-2 border-[var(--ink)] sketch-shadow-sm text-xs font-mono cursor-pointer hover:bg-[var(--erased)] disabled:opacity-60"
                style={{ borderRadius: '10px 14px 10px 14px / 14px 10px 14px 10px' }}
                title={`Export ${d.day} (${d.requests} requests)`}
              >
                {d.day} · {d.requests}
              </button>
            ))}
          </div>
        )}

        {exportFiles.length === 0 ? (
          <p className="text-sm font-body text-[var(--ink)]/60 italic">No export files yet.</p>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-left font-mono text-xs bg-[var(--surface)] border-2 border-[var(--ink)] rounded">
              <thead className="bg-[var(--erased)] border-b-2 border-[var(--ink)] font-heading text-sm">
                <tr>
                  <th className="p-2">Day</th>
                  <th className="p-2">File</th>
                  <th className="p-2">Kind</th>
                  <th className="p-2">Size</th>
                  <th className="p-2 text-right">Action</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-[var(--ink)]/20">
                {exportFiles.map((f) => (
                  <tr key={f.name}>
                    <td className="p-2">{f.day}</td>
                    <td className="p-2 break-all">{f.name}</td>
                    <td className="p-2">{f.kind}</td>
                    <td className="p-2">{(f.bytes / 1024).toFixed(1)} KB</td>
                    <td className="p-2 text-right">
                      <button
                        onClick={() => handleDelete(f.name)}
                        disabled={busy || !onDeleteExport}
                        className="inline-flex items-center gap-1 px-2 py-1 border-2 border-[var(--ink)] bg-[var(--surface)] hover:bg-[var(--tint-red)] text-[var(--marker-red)] cursor-pointer disabled:opacity-60"
                        style={{ borderRadius: '8px 12px 8px 12px / 12px 8px 12px 8px' }}
                        title="Delete this file"
                      >
                        <Trash2 className="w-3.5 h-3.5" /> Delete
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </WobblyCard>

      {/* Model Unit Cost Reference Sheet */}
      <WobblyCard variant="muted" className="p-5">
        <h4 className="text-xl font-heading font-bold text-[var(--ink)] mb-2">
          📌 Active Pricing Schema Reference (Versioned)
        </h4>
        <p className="text-sm font-body text-[var(--ink)]/80 mb-3">
          Kinetix bundles no vendor pricing assumptions. All costs are computed from your explicit rates configured in Provider settings:
        </p>

        <div className="overflow-x-auto">
          <table className="w-full text-left font-mono text-xs bg-[var(--surface)] border-2 border-[var(--ink)] rounded">
            <thead className="bg-[var(--erased)] border-b-2 border-[var(--ink)] font-heading text-sm">
              <tr>
                <th className="p-2">Model Display Name</th>
                <th className="p-2">Input / 1M</th>
                <th className="p-2">Output / 1M</th>
                <th className="p-2">Cache Read / 1M</th>
                <th className="p-2">Cache Write / 1M</th>
                <th className="p-2">Thinking / 1M</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-[var(--ink)]/20">
              {models.map((m) => (
                <tr key={m.id}>
                  <td className="p-2 font-bold">{m.displayName}</td>
                  <td className="p-2">{formatPrice(m.prices.inputPer1M, 2)}</td>
                  <td className="p-2">{formatPrice(m.prices.outputPer1M, 2)}</td>
                  <td className="p-2">{formatPrice(m.prices.cachedPer1M, 4)}</td>
                  <td className="p-2">{formatPrice(m.prices.cacheWritePer1M, 4)}</td>
                  <td className="p-2">
                    {m.capabilities.reasoning ? formatPrice(m.prices.thinkingPer1M, 2) : '—'}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </WobblyCard>
    </div>
  );
};
