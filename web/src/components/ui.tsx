import type { ReactNode } from "react";
import type { ParityStatus } from "../api/generated/ParityStatus";

export function Card({ title, actions, children }: { title: string; actions?: ReactNode; children: ReactNode }) {
  return (
    <section className="rounded-lg border border-zinc-200 bg-white dark:border-zinc-800 dark:bg-zinc-900">
      <header className="flex items-center justify-between border-b border-zinc-200 px-4 py-2.5 dark:border-zinc-800">
        <h2 className="text-sm font-semibold">{title}</h2>
        {actions}
      </header>
      <div className="p-4">{children}</div>
    </section>
  );
}

type Tone = "green" | "amber" | "red" | "zinc" | "blue";

const TONES: Record<Tone, string> = {
  green: "bg-emerald-100 text-emerald-800 dark:bg-emerald-950 dark:text-emerald-300",
  amber: "bg-amber-100 text-amber-800 dark:bg-amber-950 dark:text-amber-300",
  red: "bg-rose-100 text-rose-800 dark:bg-rose-950 dark:text-rose-300",
  zinc: "bg-zinc-100 text-zinc-700 dark:bg-zinc-800 dark:text-zinc-300",
  blue: "bg-sky-100 text-sky-800 dark:bg-sky-950 dark:text-sky-300",
};

export function Badge({ tone = "zinc", children, title }: { tone?: Tone; children: ReactNode; title?: string }) {
  return (
    <span title={title} className={`inline-block rounded px-1.5 py-0.5 text-xs font-medium whitespace-nowrap ${TONES[tone]}`}>
      {children}
    </span>
  );
}

const PARITY_TONE: Record<ParityStatus, Tone> = {
  native: "green",
  degraded: "amber",
  "needs-setup": "blue",
  impossible: "red",
  unverified: "zinc",
};

export function ParityBadge({ status }: { status: ParityStatus }) {
  return <Badge tone={PARITY_TONE[status]}>{status}</Badge>;
}

export function Button({
  children,
  onClick,
  disabled,
  variant = "default",
  title,
  type = "button",
}: {
  children: ReactNode;
  onClick?: () => void;
  disabled?: boolean;
  variant?: "default" | "ghost" | "danger";
  title?: string;
  type?: "button" | "submit";
}) {
  const base = "rounded px-2.5 py-1 text-xs font-medium disabled:opacity-50";
  const v =
    variant === "ghost"
      ? "hover:bg-zinc-100 dark:hover:bg-zinc-800"
      : variant === "danger"
        ? "bg-rose-600 text-white hover:bg-rose-500"
        : "bg-zinc-900 text-white hover:bg-zinc-700 dark:bg-zinc-100 dark:text-zinc-900 dark:hover:bg-zinc-300";
  return (
    <button type={type} className={`${base} ${v}`} onClick={onClick} disabled={disabled} title={title}>
      {children}
    </button>
  );
}

export const inputCls =
  "rounded border border-zinc-300 bg-white px-2 py-1 text-sm dark:border-zinc-700 dark:bg-zinc-900 disabled:opacity-50";

export function Field({ label, hint, children }: { label: string; hint?: ReactNode; children: ReactNode }) {
  return (
    <label className="flex flex-col gap-1 text-xs">
      <span className="font-medium text-zinc-600 dark:text-zinc-400">{label}</span>
      {children}
      {hint && <span className="text-zinc-500">{hint}</span>}
    </label>
  );
}
