// Minimal hash router: #/dashboard, #/system, #/vms/new, #/vms/<id>/<tab>.
import { useEffect, useState } from "react";

export type Route =
  | { page: "dashboard" }
  | { page: "system" }
  | { page: "new-vm" }
  | { page: "vm"; id: string; tab: string };

export function parseRoute(hash: string): Route {
  const parts = hash.replace(/^#\/?/, "").split("/").filter(Boolean);
  if (parts[0] === "system") return { page: "system" };
  if (parts[0] === "vms" && parts[1] === "new") return { page: "new-vm" };
  if (parts[0] === "vms" && parts[1]) return { page: "vm", id: parts[1], tab: parts[2] ?? "overview" };
  return { page: "dashboard" };
}

export function useRoute(): Route {
  const [route, setRoute] = useState(() => parseRoute(window.location.hash));
  useEffect(() => {
    const on = () => setRoute(parseRoute(window.location.hash));
    window.addEventListener("hashchange", on);
    return () => window.removeEventListener("hashchange", on);
  }, []);
  return route;
}

export function navigate(path: string) {
  window.location.hash = path.startsWith("#") ? path : `#${path}`;
}
