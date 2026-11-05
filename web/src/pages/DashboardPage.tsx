export default function DashboardPage() {
  return (
    <div className="max-w-2xl">
      <h1 className="text-xl font-semibold">Dashboard</h1>
      <p className="mt-2 text-sm text-zinc-600 dark:text-zinc-400">
        Virtual machines arrive in milestone M1. For now, the{" "}
        <a className="underline" href="#/system">
          System
        </a>{" "}
        page shows what this host's QEMU installation can do.
      </p>
    </div>
  );
}
