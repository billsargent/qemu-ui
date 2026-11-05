import { useEffect, useState } from "react";
import SystemPage from "./pages/SystemPage";
import DashboardPage from "./pages/DashboardPage";

// Hash routing keeps M0 dependency-free; TanStack Router arrives with M1's
// VM pages, when there are real routes with parameters.
const PAGES = {
  dashboard: { label: "Dashboard", component: DashboardPage },
  system: { label: "System", component: SystemPage },
} as const;
type PageKey = keyof typeof PAGES;

function currentPage(): PageKey {
  const key = window.location.hash.replace(/^#\/?/, "");
  return key in PAGES ? (key as PageKey) : "system";
}

export default function App() {
  const [page, setPage] = useState<PageKey>(currentPage);
  useEffect(() => {
    const onHash = () => setPage(currentPage());
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);
  const Page = PAGES[page].component;

  return (
    <div className="flex min-h-screen bg-zinc-50 text-zinc-900 dark:bg-zinc-950 dark:text-zinc-100">
      <nav className="w-52 shrink-0 border-r border-zinc-200 p-4 dark:border-zinc-800">
        <div className="mb-6 font-mono text-lg font-semibold tracking-tight">qemu-ui</div>
        <ul className="space-y-1 text-sm">
          {(Object.keys(PAGES) as PageKey[]).map((key) => (
            <li key={key}>
              <a
                href={`#/${key}`}
                className={`block rounded px-2 py-1.5 ${
                  page === key
                    ? "bg-zinc-200 font-medium dark:bg-zinc-800"
                    : "text-zinc-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-900"
                }`}
              >
                {PAGES[key].label}
              </a>
            </li>
          ))}
        </ul>
      </nav>
      <main className="min-w-0 flex-1 p-6">
        <Page />
      </main>
    </div>
  );
}
