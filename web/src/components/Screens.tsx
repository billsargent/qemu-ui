import { useEffect, useRef, useState } from "react";
import type { VmDetail } from "../api/generated/VmDetail";
import type { VmSummary } from "../api/generated/VmSummary";
import { Card } from "./ui";

/** How often a visible thumbnail asks for a fresh screendump. */
const REFRESH_MS = 5000;

const isLive = (state: string) => state === "running" || state === "paused";

/** True while `el` is on screen; thumbnails off screen cost nothing. */
function useVisible<T extends Element>() {
  const ref = useRef<T>(null);
  const [visible, setVisible] = useState(false);
  useEffect(() => {
    const el = ref.current;
    if (!el || typeof IntersectionObserver === "undefined") {
      setVisible(true);
      return;
    }
    const io = new IntersectionObserver(([e]) => setVisible(e.isIntersecting), { rootMargin: "100px" });
    io.observe(el);
    return () => io.disconnect();
  }, []);
  return { ref, visible };
}

/** A running VM's screen, refreshed while visible (M5-02). */
export function VmThumbnail({ vm }: { vm: VmSummary }) {
  const { ref, visible } = useVisible<HTMLAnchorElement>();
  const [tick, setTick] = useState(0);
  const [failed, setFailed] = useState(false);
  const live = isLive(vm.state);
  useEffect(() => {
    if (!live || !visible || document.hidden) return;
    const t = setInterval(() => !document.hidden && setTick((n) => n + 1), REFRESH_MS);
    return () => clearInterval(t);
  }, [live, visible]);
  useEffect(() => setFailed(false), [vm.state]);

  return (
    <a
      ref={ref}
      href={`#/vms/${vm.id}/${vm.has_vnc ? "console" : "overview"}`}
      className="flex aspect-[4/3] items-center justify-center overflow-hidden rounded border border-zinc-200 bg-zinc-100 text-xs text-zinc-400 dark:border-zinc-800 dark:bg-zinc-950"
    >
      {live && visible && !failed ? (
        <img
          src={`/api/v1/vms/${vm.id}/thumbnail?t=${tick}`}
          alt={`${vm.name} screen`}
          loading="lazy"
          className="h-full w-full object-contain"
          onError={() => setFailed(true)}
        />
      ) : (
        <span>{live ? (failed ? "no display" : "") : vm.state}</span>
      )}
    </a>
  );
}

/** Screenshot download and the native SPICE client handoff (M5-02, M5-05). */
export function DisplayCard({ vm }: { vm: VmDetail }) {
  const live = isLive(vm.summary.state);
  const spec = vm.live_spec ?? vm.spec;
  const spice = live && spec.display.spice;
  return (
    <Card title="Display">
      <div className="space-y-3 text-sm">
        <div className="flex flex-wrap gap-2">
          <a
            className={`rounded px-2.5 py-1 text-xs font-medium ${live ? "bg-zinc-900 text-white hover:bg-zinc-700 dark:bg-zinc-100 dark:text-zinc-900" : "pointer-events-none bg-zinc-200 text-zinc-500 dark:bg-zinc-800"}`}
            href={`/api/v1/vms/${vm.summary.id}/screenshot`}
            download
            aria-disabled={!live}
          >
            Save screenshot
          </a>
          <a
            className={`rounded px-2.5 py-1 text-xs font-medium ${spice ? "bg-zinc-900 text-white hover:bg-zinc-700 dark:bg-zinc-100 dark:text-zinc-900" : "pointer-events-none bg-zinc-200 text-zinc-500 dark:bg-zinc-800"}`}
            href={`/api/v1/vms/${vm.summary.id}/console.vv`}
            download="console.vv"
            aria-disabled={!spice}
          >
            Open in remote-viewer
          </a>
        </div>
        <p className="text-xs text-zinc-500">
          {spec.display.spice
            ? "remote-viewer (virt-viewer) gives smoother video and USB redirection. The file works once, on this computer, while the VM runs."
            : "Turn on SPICE in Settings → Display to use a native client (remote-viewer) for smoother video and USB redirection."}
        </p>
      </div>
    </Card>
  );
}
