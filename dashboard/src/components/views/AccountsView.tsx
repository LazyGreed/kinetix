import React, { useState } from 'react';
import { Users, Plus, ShieldCheck, Clock, AlertTriangle, RefreshCw, KeyRound, Sparkles, Trash2, Pencil } from 'lucide-react';
import { Account, Provider } from '../../types';
import { WobblyCard, SketchButton, SketchBadge } from '../HandDrawnElements';
import { formatCurrency, formatTokens, DESIGN_TOKENS } from '../../lib/designSystem';
import { Kinetix } from '../../lib/resources';

interface AccountsViewProps {
  accounts: Account[];
  providers: Provider[];
  onAddAccount: (acc: Account & { apiKey?: string }) => void;
  onUpdateAccount: (acc: Account) => void;
  onDeleteAccount: (accountId: string) => void;
}

export const AccountsView: React.FC<AccountsViewProps> = ({
  accounts,
  providers,
  onAddAccount,
  onUpdateAccount,
  onDeleteAccount,
}) => {
  const [showAddModal, setShowAddModal] = useState(false);
  const [confirmDeleteAccountId, setConfirmDeleteAccountId] = useState<string | null>(null);
  const [providerFilter, setProviderFilter] = useState('all');
  const [editingAccount, setEditingAccount] = useState<Account | null>(null);
  const [editLabel, setEditLabel] = useState('');
  const [label, setLabel] = useState('');
  const [providerId, setProviderId] = useState(providers[0]?.id || '');
  const [apiKey, setApiKey] = useState('');
  const [softQuota, setSoftQuota] = useState(100);
  const [accountValidation, setAccountValidation] = useState<{ valid: boolean; problems: string[] } | null>(null);
  const [validatingAccount, setValidatingAccount] = useState(false);

  const handleValidateAccount = async () => {
    const prov = providers.find((p) => p.id === providerId) || providers[0];
    if (!label.trim() || !apiKey.trim() || !prov) return;
    setValidatingAccount(true);
    try {
      const r = await Kinetix.validateAccount({
        provider_id: prov.id,
        label: label.trim(),
        api_key: apiKey.trim(),
        quota_type: 'monthly',
      });
      setAccountValidation({ valid: r.valid, problems: r.problems || [] });
    } catch (e) {
      setAccountValidation({ valid: false, problems: [(e as Error).message] });
    } finally {
      setValidatingAccount(false);
    }
  };

  const handleCreateSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!label.trim() || !apiKey.trim()) return;

    const prov = providers.find((p) => p.id === providerId) || providers[0];
    const masked = apiKey.slice(0, 6) + '...' + apiKey.slice(-4);

    const newAcc: Account & { apiKey?: string } = {
      id: '',
      providerId: prov.id,
      providerName: prov.name,
      label: label.trim(),
      keyMasked: masked,
      status: 'healthy',
      quotaType: 'monthly',
      softQuotaSpendLimit: softQuota,
      currentSpend: 0,
      requestsCount: 0,
      tokensCount: 0,
      priority: accounts.filter((a) => a.providerId === prov.id).length + 1,
      apiKey: apiKey.trim(),
    };

    onAddAccount(newAcc);
    setShowAddModal(false);
    setLabel('');
    setApiKey('');
  };

  const handleResetCooldown = (acc: Account) => {
    onUpdateAccount({
      ...acc,
      status: 'healthy',
      cooldownUntil: null,
      lastError: undefined,
    });
  };

  const visibleProviders = providers.filter((provider) =>
    providerFilter === 'all' || provider.id === providerFilter,
  );
  const orphanAccounts = accounts.filter(
    (account) => !providers.some((provider) => provider.id === account.providerId),
  );

  const openAddForProvider = (id: string) => {
    setProviderId(id);
    setShowAddModal(true);
  };

  const openRename = (acc: Account) => {
    setEditingAccount(acc);
    setEditLabel(acc.label);
  };

  const handleRenameSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    if (!editingAccount || !editLabel.trim()) return;
    onUpdateAccount({ ...editingAccount, label: editLabel.trim() });
    setEditingAccount(null);
    setEditLabel('');
  };

  return (
    <div className="space-y-6">
      {/* Top Banner */}
      <div className="flex flex-col md:flex-row items-start md:items-center justify-between gap-4">
        <div>
          <h2 className="text-3xl font-heading font-bold text-[var(--ink)] flex items-center gap-2">
            <span>Key Pool & Accounts Health</span>
            <SketchBadge variant="yellow" rotation="-1deg">
              FR-4 & FR-12
            </SketchBadge>
          </h2>
          <p className="text-base font-body text-[var(--ink)]/80">
            Accounts hold real upstream API keys securely. Individual keys cycle into cooldown on 429s or quota exhaustion without interrupting client requests.
          </p>
        </div>

        <SketchButton
          variant="primary"
          size="md"
          onClick={() => setShowAddModal(true)}
          className="gap-2 font-heading font-bold"
        >
          <Plus className="w-5 h-5" />
          Add Upstream Credential
        </SketchButton>
      </div>

      {providers.length > 1 && (
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-xs font-mono text-[var(--ink)]/60">Pool:</span>
          <button
            onClick={() => setProviderFilter('all')}
            className={`px-3 py-1.5 text-xs font-heading font-bold border border-[var(--ink)] rounded cursor-pointer ${
              providerFilter === 'all' ? 'bg-[var(--postit)] sketch-shadow-sm' : 'bg-[var(--surface)]'
            }`}
          >
            All Providers
          </button>
          {providers.map((provider) => (
            <button
              key={provider.id}
              onClick={() => setProviderFilter(provider.id)}
              className={`px-3 py-1.5 text-xs font-heading font-bold border border-[var(--ink)] rounded cursor-pointer ${
                providerFilter === provider.id ? 'bg-[var(--postit)] sketch-shadow-sm' : 'bg-[var(--surface)]'
              }`}
            >
              {provider.name}
            </button>
          ))}
        </div>
      )}

      {/* Provider account pools */}
      {accounts.length === 0 ? (
        <WobblyCard decoration="tack" className="p-10 text-center bg-[var(--surface)]">
          <KeyRound className="w-12 h-12 text-[var(--pen-blue)] mx-auto mb-3 opacity-60" />
          <h3 className="text-2xl font-heading font-bold text-[var(--ink)]">No Account Credentials or Pools Configured</h3>
          <p className="text-base font-body text-[var(--ink)]/80 max-w-lg mx-auto mt-2 mb-6">
            Store multiple API keys and upstream accounts per provider. Kinetix groups them into active pools, tracks spend against soft quotas, and isolates keys from client applications.
          </p>
          <SketchButton
            variant="primary"
            size="md"
            onClick={() => setShowAddModal(true)}
            className="gap-2 font-heading font-bold"
          >
            <Plus className="w-5 h-5" />
            Add First Upstream Credential
          </SketchButton>
        </WobblyCard>
      ) : (
        <div className="space-y-8">
          {visibleProviders.map((provider) => {
            const poolAccounts = accounts.filter((acc) => acc.providerId === provider.id);
            const healthy = poolAccounts.filter((acc) => acc.status === 'healthy').length;
            const cooldown = poolAccounts.filter((acc) => acc.status === 'cooldown').length;
            const exhausted = poolAccounts.filter((acc) => acc.status === 'exhausted').length;
            return (
              <section key={provider.id} className="space-y-4">
                <div className="flex flex-wrap items-center justify-between gap-3 border-b-2 border-dashed border-[var(--ink)]/25 pb-2">
                  <div>
                    <h3 className="text-xl font-heading font-bold text-[var(--ink)]">{provider.name} Pool</h3>
                    <p className="text-xs font-mono text-[var(--ink)]/65">
                      {poolAccounts.length} credential(s) · {healthy} healthy · {cooldown} cooldown · {exhausted} exhausted
                    </p>
                  </div>
                  <SketchButton variant="secondary" size="sm" onClick={() => openAddForProvider(provider.id)} className="gap-1">
                    <Plus className="w-4 h-4" />
                    Add Credential
                  </SketchButton>
                </div>

                {poolAccounts.length === 0 ? (
                  <div className="p-5 text-sm font-body text-[var(--ink)]/65 bg-[var(--surface)] border-2 border-dashed border-[var(--ink)]/25 rounded">
                    No credentials in this provider pool.
                  </div>
                ) : (
                  <div className="grid grid-cols-1 md:grid-cols-2 gap-6">
                  {poolAccounts.map((acc, idx) => {
                    const isCooldown = acc.status === 'cooldown';
                    const isExhausted = acc.status === 'exhausted';
                    const rotation = idx % 2 === 0 ? '-0.5deg' : '0.5deg';

                    return (
              <WobblyCard
                key={acc.id}
                decoration={isCooldown ? 'tack' : idx % 2 === 0 ? 'tape' : 'none'}
                rotation={rotation}
                className={`p-5 flex flex-col justify-between ${
                  isCooldown ? 'bg-[var(--tint-red)]' : isExhausted ? 'bg-[var(--tint-yellow)]' : 'bg-[var(--surface)]'
                }`}
              >
                <div>
                  <div className="flex items-start justify-between gap-2 mb-2">
                    <div>
                      <span className="text-xs font-mono bg-[var(--erased)] px-2 py-0.5 rounded border border-[var(--ink)]/30 inline-block mb-1">
                        {acc.providerName}
                      </span>
                      <h3 className="text-xl font-heading font-bold text-[var(--ink)] flex items-center gap-2">
                        <KeyRound className="w-5 h-5 text-[var(--pen-blue)]" />
                        {acc.label}
                      </h3>
                    </div>

                    {acc.status === 'healthy' ? (
                      <SketchBadge variant="green" rotation="1deg">
                        Healthy
                      </SketchBadge>
                    ) : acc.status === 'cooldown' ? (
                      <SketchBadge variant="red" rotation="-1deg">
                        In Cooldown (429)
                      </SketchBadge>
                    ) : (
                      <SketchBadge variant="yellow" rotation="1deg">
                        Quota Exhausted
                      </SketchBadge>
                    )}
                  </div>

                  {/* Key masked preview */}
                  <div className="flex items-center justify-between bg-[var(--paper)] p-2 border-2 border-dashed border-[var(--ink)] text-xs font-mono mb-4">
                    <span>Masked Secret: <strong>{acc.keyMasked}</strong></span>
                    <span className="text-[var(--pen-green)] font-bold">🔒 Encrypted at rest</span>
                  </div>

                  {/* Cooldown / Quota warning notice */}
                  {isCooldown && (
                    <div className="p-3 bg-[var(--tint-red)] border-2 border-[var(--marker-red)] sketch-shadow-sm mb-4 rounded text-xs font-mono text-[var(--danger-text)]">
                      <div className="flex items-center gap-1.5 font-bold mb-1">
                        <AlertTriangle className="w-4 h-4" />
                        <span>{acc.lastError || 'Rate Limit (HTTP 429)'}</span>
                      </div>
                      <div>Cooling down: {acc.cooldownUntil || 'until Retry-After passes'}</div>
                      <button
                        onClick={() => handleResetCooldown(acc)}
                        className="mt-2 px-2 py-1 bg-[var(--surface)] border border-[var(--ink)] text-[var(--ink)] hover:bg-[var(--erased)] rounded flex items-center gap-1 cursor-pointer font-bold"
                      >
                        <RefreshCw className="w-3 h-3" />
                        Clear Cooldown & Probe Upstream
                      </button>
                    </div>
                  )}

                  {/* Soft Quotas & Statistics */}
                  <div className="grid grid-cols-2 gap-2 text-xs font-mono bg-[var(--surface)] p-3 border border-[var(--ink)] rounded mb-4">
                    <div>
                      Requests Served: <strong>{acc.requestsCount.toLocaleString()}</strong>
                    </div>
                    <div>
                      Tokens Processed: <strong>{formatTokens(acc.tokensCount)}</strong>
                    </div>
                    <div>
                      Spend Accrued: <strong>{formatCurrency(acc.currentSpend)}</strong>
                    </div>
                    <div>
                      Soft Quota Cap:{' '}
                      <strong>
                        {acc.softQuotaSpendLimit ? formatCurrency(acc.softQuotaSpendLimit) : 'Unlimited'}
                      </strong>
                    </div>
                    {acc.quotaResetTime && (
                      <div className="col-span-2 text-[var(--pen-blue)] pt-1 border-t border-[var(--ink)]/20">
                        Quota Reset: <strong>{acc.quotaResetTime}</strong>
                      </div>
                    )}
                  </div>
                </div>

                {/* Footer */}
                <div className="flex flex-wrap items-center justify-between gap-2 border-t border-[var(--ink)]/20 pt-3 text-xs font-mono">
                  <div className="text-[var(--ink)]/70">
                    <span>Provider Priority: <strong>Tier #{acc.priority}</strong></span>
                  </div>

                  <div className="flex items-center gap-2 ml-auto">
                    <button
                      onClick={() => openRename(acc)}
                      className="px-2 py-1 text-xs font-heading font-bold text-[var(--pen-blue)] hover:bg-[var(--tint-blue)] border border-[var(--pen-blue)]/40 hover:border-[var(--pen-blue)] rounded flex items-center gap-1 cursor-pointer transition-colors"
                      title="Rename this account"
                    >
                      <Pencil className="w-3.5 h-3.5" />
                      <span>Rename</span>
                    </button>

                  {confirmDeleteAccountId === acc.id ? (
                    <div className="flex items-center gap-1 bg-[var(--tint-red)] px-2 py-1 border border-[var(--marker-red)] rounded text-xs font-heading">
                      <span className="text-[var(--danger-text)] font-bold">Remove pool key?</span>
                      <button
                        onClick={() => {
                          onDeleteAccount(acc.id);
                          setConfirmDeleteAccountId(null);
                        }}
                        className="px-2 py-0.5 bg-[var(--marker-red)] text-[var(--surface)] rounded font-bold hover:brightness-90 cursor-pointer"
                      >
                        Confirm
                      </button>
                      <button
                        onClick={() => setConfirmDeleteAccountId(null)}
                        className="px-2 py-0.5 bg-[var(--surface)] border border-[var(--ink)] rounded hover:bg-[var(--erased)] cursor-pointer"
                      >
                        Cancel
                      </button>
                    </div>
                  ) : (
                    <button
                      onClick={() => setConfirmDeleteAccountId(acc.id)}
                      className="px-2 py-1 text-xs font-heading font-bold text-[var(--marker-red)] hover:bg-[var(--tint-red)] border border-[var(--marker-red)]/40 hover:border-[var(--marker-red)] rounded flex items-center gap-1 cursor-pointer transition-colors"
                      title="Remove this credential account from pool"
                    >
                      <Trash2 className="w-3.5 h-3.5" />
                      <span>Remove Pool Key</span>
                    </button>
                  )}
                  </div>
                </div>
              </WobblyCard>
                    );
                  })}
                  </div>
                )}
              </section>
            );
          })}

          {providerFilter === 'all' && orphanAccounts.length > 0 && (
            <section className="space-y-3">
              <h3 className="text-xl font-heading font-bold text-[var(--ink)]">Unknown Provider Pool</h3>
              <div className="grid grid-cols-1 md:grid-cols-2 gap-6">
                {orphanAccounts.map((acc) => (
                  <WobblyCard key={acc.id} className="p-5 bg-[var(--surface)]">
                    <h4 className="font-heading font-bold">{acc.label}</h4>
                    <p className="text-xs font-mono">{acc.providerId} · Tier #{acc.priority} · {acc.status}</p>
                  </WobblyCard>
                ))}
              </div>
            </section>
          )}
        </div>
      )}

      {editingAccount && (
        <div className="fixed inset-0 z-50 flex items-center justify-center p-4 bg-black/40 backdrop-blur-xs">
          <div className="w-full max-w-md">
            <WobblyCard decoration="tape" className="bg-[var(--paper)] p-6 relative">
              <button
                onClick={() => setEditingAccount(null)}
                className="absolute top-4 right-4 text-[var(--ink)] font-bold text-xl hover:text-[var(--marker-red)] cursor-pointer"
              >
                ✕
              </button>
              <h3 className="text-2xl font-heading font-bold text-[var(--ink)] mb-4 flex items-center gap-2">
                <Pencil className="w-5 h-5 text-[var(--pen-blue)]" />
                Rename Account
              </h3>
              <form onSubmit={handleRenameSubmit} className="space-y-4 font-body">
                <div>
                  <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                    Account Label
                  </label>
                  <input
                    autoFocus
                    type="text"
                    required
                    value={editLabel}
                    onChange={(e) => setEditLabel(e.target.value)}
                    className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base sketch-shadow-sm focus:outline-none"
                    style={{ borderRadius: DESIGN_TOKENS.radii.wobbly }}
                  />
                  {!editLabel.trim() && (
                    <p className="text-xs text-[var(--danger-text)] mt-1">Label cannot be blank.</p>
                  )}
                </div>
                <div className="flex justify-end gap-3">
                  <SketchButton type="button" variant="ghost" onClick={() => setEditingAccount(null)}>
                    Cancel
                  </SketchButton>
                  <SketchButton type="submit" variant="primary" disabled={!editLabel.trim()}>
                    Save
                  </SketchButton>
                </div>
              </form>
            </WobblyCard>
          </div>
        </div>
      )}

      {/* Add Account Modal */}
      {showAddModal && (
        <div className="fixed inset-0 z-50 flex items-center justify-center p-4 bg-black/40 backdrop-blur-xs">
          <div className="w-full max-w-lg">
            <WobblyCard decoration="tape" className="bg-[var(--paper)] p-6 relative">
              <button
                onClick={() => setShowAddModal(false)}
                className="absolute top-4 right-4 text-[var(--ink)] font-bold text-xl hover:text-[var(--marker-red)] cursor-pointer"
              >
                ✕
              </button>

              <h3 className="text-2xl font-heading font-bold text-[var(--ink)] mb-4 flex items-center gap-2">
                <KeyRound className="w-6 h-6 text-[var(--pen-blue)]" />
                Add Upstream Provider Credential
              </h3>

              <form onSubmit={handleCreateSubmit} className="space-y-4 font-body">
                <div>
                  <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                    Provider
                  </label>
                  <select
                    value={providerId}
                    onChange={(e) => setProviderId(e.target.value)}
                    className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base font-body sketch-shadow-sm focus:outline-none"
                    style={{ borderRadius: DESIGN_TOKENS.radii.wobblyMd }}
                  >
                    {providers.map((p) => (
                      <option key={p.id} value={p.id}>
                        {p.name} ({p.wireFormat})
                      </option>
                    ))}
                  </select>
                </div>

                <div>
                  <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                    Account Label / Identification
                  </label>
                  <input
                    type="text"
                    required
                    placeholder="e.g. Gemini Team Pay-as-you-go #2"
                    value={label}
                    onChange={(e) => setLabel(e.target.value)}
                    className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base sketch-shadow-sm focus:outline-none"
                    style={{ borderRadius: DESIGN_TOKENS.radii.wobbly }}
                  />
                </div>

                <div>
                  <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                    Raw API Key (Encrypted immediately on storage)
                  </label>
                  <input
                    type="password"
                    required
                    placeholder="AIzaSy... or sk-..."
                    value={apiKey}
                    onChange={(e) => setApiKey(e.target.value)}
                    className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base font-mono sketch-shadow-sm focus:outline-none"
                    style={{ borderRadius: DESIGN_TOKENS.radii.wobblyMd }}
                  />
                  <p className="text-xs text-[var(--ink)]/60 mt-1">
                    Upstream keys never leave the server or appear in client responses.
                  </p>
                </div>

                <div>
                  <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                    Soft Quota Spend Limit (USD/month)
                  </label>
                  <input
                    type="number"
                    value={softQuota}
                    onChange={(e) => setSoftQuota(Number(e.target.value))}
                    className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base sketch-shadow-sm focus:outline-none"
                    style={{ borderRadius: DESIGN_TOKENS.radii.wobblyBtn }}
                  />
                </div>

                <div className="pt-2 flex justify-end gap-3">
                  <SketchButton
                    type="button"
                    variant="ghost"
                    onClick={() => setShowAddModal(false)}
                  >
                    Cancel
                  </SketchButton>
                  <SketchButton
                    type="button"
                    variant="secondary"
                    onClick={handleValidateAccount}
                    disabled={validatingAccount || !label.trim() || !apiKey.trim()}
                  >
                    {validatingAccount ? 'Validating…' : 'Validate (Dry Run)'}
                  </SketchButton>
                  <SketchButton type="submit" variant="danger" className="font-bold">
                    Save Key to Pool
                  </SketchButton>
                </div>
                {accountValidation && (
                  <div
                    className="mt-3 p-3 text-sm font-mono"
                    style={{
                      borderRadius: DESIGN_TOKENS.radii.wobbly,
                      background: accountValidation.valid ? 'var(--tint-green)' : 'var(--tint-red)',
                      border: `2px solid ${accountValidation.valid ? 'var(--pen-green)' : 'var(--marker-red)'}`,
                    }}
                  >
                    <div className="font-bold mb-1">
                      {accountValidation.valid ? 'Validate: passed' : 'Validate: problems found'}
                    </div>
                    {accountValidation.problems.map((p, i) => (
                      <div key={i} style={{ color: 'var(--danger-text)' }}>• {p}</div>
                    ))}
                  </div>
                )}
              </form>
            </WobblyCard>
          </div>
        </div>
      )}
    </div>
  );
};
