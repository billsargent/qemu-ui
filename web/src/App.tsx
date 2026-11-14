import { useQuery } from "@tanstack/react-query";
import { api } from "./api/client";
import { useLiveInvalidation, useEventStreamConnected } from "./api/events";
import JobsPage from "./pages/JobsPage";
import StoragePage from "./pages/StoragePage";
import DashboardPage from "./pages/DashboardPage";
import NewVmPage from "./pages/NewVmPage";
import SystemPage from "./pages/SystemPage";
import VmPage from "./pages/VmPage";
import { useRoute } from "./router";

export default function App() {
  const route = useRoute();
  useLiveInvalidation();
  const live = useEventStreamConnected();
  const jobs = useQuery({ queryKey: ["jobs"], queryFn: api.jobs });
  const activeJobs = (jobs.data ?? []).filter((j) => j.status === "queued" || j.status === "running").length;

  const nav = [
    { href: "#/dashboard", label: "Virtual machines", active: route.page === "dashboard" || route.page === "vm" || route.page === "new-vm" },
    { href: "#/storage", label: "Storage", active: route.page === "storage" },
    { href: "#/jobs", label: activeJobs ? `Jobs (${activeJobs})` : "Jobs", active: route.page === "jobs" },
    { href: "#/system", label: "System", active: route.page === "system" },
  ];

  return (
    <div className="flex min-h-screen bg-zinc-50 text-zinc-900 dark:bg-zinc-950 dark:text-zinc-100">
      <nav className="flex w-52 shrink-0 flex-col border-r border-zinc-200 p-4 dark:border-zinc-800">
        <div className="mb-6 font-mono text-lg font-semibold tracking-tight">qemu-ui</div>
        <ul className="space-y-1 text-sm">
          {nav.map((n) => (
            <li key={n.href}>
              <a
                href={n.href}
                className={`block rounded px-2 py-1.5 ${
                  n.active
                    ? "bg-zinc-200 font-medium dark:bg-zinc-800"
                    : "text-zinc-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-900"
                }`}
              >
                {n.label}
              </a>
            </li>
          ))}
        </ul>
        <div className="mt-auto flex items-center gap-2 text-xs text-zinc-500" title="Live updates from the daemon">
          <span className={`inline-block h-2 w-2 rounded-full ${live ? "bg-emerald-500" : "bg-zinc-400"}`} />
          {live ? "live" : "reconnecting…"}
        </div>
      </nav>
      <main className="min-w-0 flex-1 p-6">
        {route.page === "dashboard" && <DashboardPage />}
        {route.page === "system" && <SystemPage />}
        {route.page === "storage" && <StoragePage pool={route.pool} image={route.image} />}
        {route.page === "jobs" && <JobsPage />}
        {route.page === "new-vm" && <NewVmPage />}
        {route.page === "vm" && <VmPage key={route.id} id={route.id} tab={route.tab} />}
      </main>
    </div>
  );
}
