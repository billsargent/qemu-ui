import { useEffect, useState, type ReactNode } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { AUTH_REQUIRED, api, setCsrf, type AuthState } from "../api/client";
import { Button, Card, Field, inputCls } from "./ui";

export function useAuthState() {
  return useQuery({
    queryKey: ["auth"],
    queryFn: api.authState,
    staleTime: 60_000,
  });
}

function Centered({ title, children }: { title: string; children: ReactNode }) {
  return (
    <div className="flex min-h-screen items-center justify-center bg-zinc-50 p-4 text-zinc-900 dark:bg-zinc-950 dark:text-zinc-100">
      <div className="w-full max-w-sm rounded-lg border border-zinc-200 bg-white p-6 shadow-sm dark:border-zinc-800 dark:bg-zinc-900">
        <div className="mb-1 font-mono text-lg font-semibold">qemu-ui</div>
        <h1 className="mb-4 text-sm text-zinc-500">{title}</h1>
        {children}
      </div>
    </div>
  );
}

function CredentialsForm({
  submit,
  label,
  confirm,
  defaultUser,
}: {
  submit: (user: string, password: string) => Promise<unknown>;
  label: string;
  confirm?: boolean;
  defaultUser?: string;
}) {
  const [user, setUser] = useState(defaultUser ?? "");
  const [pw, setPw] = useState("");
  const [pw2, setPw2] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const mismatch = confirm && pw2 !== "" && pw !== pw2;
  return (
    <form
      className="space-y-3"
      onSubmit={async (e) => {
        e.preventDefault();
        if (confirm && pw !== pw2) return;
        setBusy(true);
        setError(null);
        try {
          await submit(user, pw);
        } catch (err) {
          setError((err as Error).message);
        } finally {
          setBusy(false);
        }
      }}
    >
      <Field label="User name">
        <input
          className={`${inputCls} w-full`}
          autoComplete="username"
          value={user}
          onChange={(e) => setUser(e.target.value)}
        />
      </Field>
      <Field
        label="Password"
        hint={confirm ? "at least 8 characters" : undefined}
      >
        <input
          className={`${inputCls} w-full`}
          type="password"
          autoComplete={confirm ? "new-password" : "current-password"}
          value={pw}
          onChange={(e) => setPw(e.target.value)}
        />
      </Field>
      {confirm && (
        <Field
          label="Repeat password"
          hint={mismatch ? "the passwords differ" : undefined}
        >
          <input
            className={`${inputCls} w-full`}
            type="password"
            autoComplete="new-password"
            value={pw2}
            onChange={(e) => setPw2(e.target.value)}
          />
        </Field>
      )}
      {error && <p className="text-xs text-rose-600">{error}</p>}
      <Button
        type="submit"
        disabled={busy || !user || !pw || (confirm && pw !== pw2)}
      >
        {label}
      </Button>
    </form>
  );
}

/** First-run setup: create the admin account (this machine only). */
export function SetupPage({ onDone }: { onDone: () => void }) {
  return (
    <Centered title="First-run setup: create the admin account">
      <p className="mb-4 text-xs text-zinc-500">
        This account protects the daemon. Until it exists, qemu-ui only answers
        on this machine. Afterwards you can let it listen on the network (
        <code>--listen</code>), ideally with TLS.
      </p>
      <CredentialsForm
        label="Create account"
        confirm
        submit={async (u, p) => {
          const r = await api.setup(u, p);
          setCsrf(r.csrf);
          onDone();
        }}
      />
      <p className="mt-4 text-xs">
        <a className="underline" href="#/dashboard">
          Later
        </a>
      </p>
    </Centered>
  );
}

function LoginPage({
  state,
  onDone,
}: {
  state: AuthState;
  onDone: () => void;
}) {
  return (
    <Centered title="Log in">
      <CredentialsForm
        label="Log in"
        defaultUser={state.username ?? ""}
        submit={async (u, p) => {
          const r = await api.login(u, p);
          setCsrf(r.csrf);
          onDone();
        }}
      />
    </Centered>
  );
}

/**
 * Shows login when the daemon wants one, the setup page at `#/setup`, and
 * otherwise the app. Keeps the CSRF token in step with the session.
 */
export function AuthGate({
  children,
  setupRoute,
}: {
  children: ReactNode;
  setupRoute: boolean;
}) {
  const qc = useQueryClient();
  const auth = useAuthState();
  useEffect(() => {
    if (auth.data) setCsrf(auth.data.csrf);
  }, [auth.data]);
  useEffect(() => {
    const on = () => qc.invalidateQueries({ queryKey: ["auth"] });
    window.addEventListener(AUTH_REQUIRED, on);
    return () => window.removeEventListener(AUTH_REQUIRED, on);
  }, [qc]);
  const refresh = async () => {
    await qc.invalidateQueries({ queryKey: ["auth"] });
    await qc.invalidateQueries();
  };
  if (!auth.data) return null;
  if (!auth.data.authenticated) {
    if (!auth.data.setup_complete) return <SetupPage onDone={refresh} />;
    return <LoginPage state={auth.data} onDone={refresh} />;
  }
  if (setupRoute && !auth.data.setup_complete)
    return (
      <SetupPage
        onDone={async () => {
          window.location.hash = "#/dashboard";
          await refresh();
        }}
      />
    );
  return <>{children}</>;
}

/** Reminder shown until the admin account exists. */
export function SetupBanner() {
  const auth = useAuthState();
  if (!auth.data || auth.data.setup_complete) return null;
  return (
    <div className="mb-4 rounded border border-amber-300 bg-amber-50 px-3 py-2 text-sm text-amber-900 dark:border-amber-800 dark:bg-amber-950 dark:text-amber-200">
      No admin account yet: anyone using this machine can control qemu-ui, and
      it can't be opened from the network.{" "}
      <a className="font-medium underline" href="#/setup">
        Create the admin account
      </a>
    </div>
  );
}

/** Password, API tokens, expert mode, logout (Settings → Security). */
export function SecurityCard() {
  const qc = useQueryClient();
  const auth = useAuthState();
  const tokens = useQuery({
    queryKey: ["api-tokens"],
    queryFn: api.apiTokens,
    enabled: !!auth.data?.setup_complete && auth.data.actor !== "anonymous",
  });
  const [cur, setCur] = useState("");
  const [next, setNext] = useState("");
  const [tokenName, setTokenName] = useState("");
  const [newToken, setNewToken] = useState<string | null>(null);
  const pw = useMutation({
    mutationFn: () => api.changePassword(cur, next),
    onSuccess: () => {
      setCur("");
      setNext("");
    },
  });
  const create = useMutation({
    mutationFn: () => api.createApiToken(tokenName),
    onSuccess: (r) => {
      setNewToken(r.token);
      setTokenName("");
      qc.invalidateQueries({ queryKey: ["api-tokens"] });
    },
  });
  const revoke = useMutation({
    mutationFn: (id: string) => api.revokeApiToken(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["api-tokens"] }),
  });
  const expert = useMutation({
    mutationFn: (on: boolean) => api.setExpertMode(on),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["auth"] }),
  });
  const logout = useMutation({
    mutationFn: api.logout,
    onSuccess: () => {
      setCsrf(null);
      qc.invalidateQueries();
    },
  });
  const a = auth.data;
  if (!a) return null;
  return (
    <Card title="Security">
      <div className="space-y-5 text-sm">
        <div className="flex flex-wrap items-center gap-2">
          <span>
            Signed in as{" "}
            <span className="font-medium">
              {a.setup_complete ? a.username : "local user (no account yet)"}
            </span>
          </span>
          <span className="text-xs text-zinc-500">
            {a.tls ? "over TLS" : "without TLS"}
          </span>
          {a.actor === "admin" && (
            <span className="ml-auto">
              <Button variant="ghost" onClick={() => logout.mutate()}>
                Log out
              </Button>
            </span>
          )}
        </div>
        {!a.setup_complete ? (
          <p className="text-xs text-zinc-500">
            <a className="underline" href="#/setup">
              Create the admin account
            </a>{" "}
            to set a password and API tokens.
          </p>
        ) : (
          <>
            <form
              className="flex flex-wrap items-end gap-2"
              onSubmit={(e) => {
                e.preventDefault();
                pw.mutate();
              }}
            >
              <Field label="Current password">
                <input
                  className={inputCls}
                  type="password"
                  autoComplete="current-password"
                  value={cur}
                  onChange={(e) => setCur(e.target.value)}
                />
              </Field>
              <Field
                label="New password"
                hint="at least 8 characters; other browsers are logged out"
              >
                <input
                  className={inputCls}
                  type="password"
                  autoComplete="new-password"
                  value={next}
                  onChange={(e) => setNext(e.target.value)}
                />
              </Field>
              <Button type="submit" disabled={!cur || !next || pw.isPending}>
                Change password
              </Button>
              {pw.isSuccess && (
                <span className="text-xs text-emerald-600">changed</span>
              )}
              {pw.error && (
                <span className="text-xs text-rose-600">
                  {pw.error.message}
                </span>
              )}
            </form>

            <div>
              <div className="mb-1 font-medium">API tokens</div>
              <p className="mb-2 text-xs text-zinc-500">
                For scripts and the CLI: send as{" "}
                <code>Authorization: Bearer …</code>.
              </p>
              <ul className="mb-2 space-y-1">
                {(tokens.data ?? []).map((t) => (
                  <li key={t.id} className="flex items-center gap-2 text-xs">
                    <span className="font-medium">{t.name}</span>
                    <span className="text-zinc-500">
                      created{" "}
                      {new Date(t.created_at * 1000).toLocaleDateString()} ·{" "}
                      {t.last_used
                        ? `used ${new Date(t.last_used * 1000).toLocaleString()}`
                        : "never used"}
                    </span>
                    <span className="ml-auto">
                      <Button
                        variant="ghost"
                        onClick={() => revoke.mutate(t.id)}
                      >
                        Revoke
                      </Button>
                    </span>
                  </li>
                ))}
              </ul>
              <form
                className="flex items-end gap-2"
                onSubmit={(e) => {
                  e.preventDefault();
                  create.mutate();
                }}
              >
                <Field label="New token name">
                  <input
                    className={inputCls}
                    value={tokenName}
                    onChange={(e) => setTokenName(e.target.value)}
                    placeholder="backup script"
                  />
                </Field>
                <Button type="submit" disabled={!tokenName || create.isPending}>
                  Create token
                </Button>
              </form>
              {newToken && (
                <p className="mt-2 text-xs">
                  Copy it now; it won't be shown again:{" "}
                  <code className="select-all break-all font-mono">
                    {newToken}
                  </code>
                </p>
              )}
              {(create.error ?? revoke.error ?? tokens.error) && (
                <p className="text-xs text-rose-600">
                  {(create.error ?? revoke.error ?? tokens.error)?.message}
                </p>
              )}
            </div>
          </>
        )}

        <div>
          <label className="flex items-center gap-2">
            <input
              type="checkbox"
              checked={a.expert_mode}
              disabled={expert.isPending}
              onChange={(e) => expert.mutate(e.target.checked)}
            />
            <span className="font-medium">Expert mode</span>
          </label>
          <p className="mt-1 text-xs text-zinc-500">
            Unlocks raw QEMU passthrough arguments and the QMP console. Both are
            root-equivalent on the host. Turning it on or off, and every use, is
            audited.
          </p>
          {expert.error && (
            <p className="text-xs text-rose-600">{expert.error.message}</p>
          )}
        </div>
      </div>
    </Card>
  );
}
