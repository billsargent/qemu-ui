import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { api } from "../api/client";
import { Badge, Card } from "./ui";

const STEPS: Record<string, string[]> = {
  Windows: [
    "Download the certificate below.",
    "Double-click it → Install Certificate → Current User.",
    "Choose “Place all certificates in the following store” → Trusted Root Certification Authorities.",
    "Restart the browser. Firefox keeps its own list: Settings → Privacy & Security → Certificates → View Certificates → Authorities → Import.",
  ],
  macOS: [
    "Download the certificate below and open it; Keychain Access adds it to the login keychain.",
    "Double-click the certificate in Keychain Access → Trust → “When using this certificate: Always Trust”.",
    "Restart the browser.",
  ],
  Linux: [
    "Download the certificate below.",
    "Debian/Ubuntu: copy it to /usr/local/share/ca-certificates/qemu-ui-ca.crt and run `sudo update-ca-certificates`.",
    "Fedora/RHEL: copy it to /etc/pki/ca-trust/source/anchors/ and run `sudo update-ca-trust`.",
    "Chrome and Firefox may keep their own lists: import it under their certificate settings as an authority.",
  ],
  "Android / iOS": [
    "Android: Settings → Security → Encryption & credentials → Install a certificate → CA certificate.",
    "iOS: open the downloaded file, install the profile in Settings, then enable full trust under General → About → Certificate Trust Settings.",
  ],
};

/** How the daemon is served, and how to trust its certificate. */
export function TlsCard() {
  const q = useQuery({ queryKey: ["tls"], queryFn: api.tls });
  const [os, setOs] = useState("Windows");
  if (!q.data) return null;
  const t = q.data;
  if (!t.enabled) {
    return (
      <Card title="HTTPS">
        <p className="text-sm text-zinc-500">
          Off: the daemon serves plain HTTP on loopback only. It switches to HTTPS automatically when it listens on the network (or with{" "}
          <code>--tls on</code>).
        </p>
      </Card>
    );
  }
  return (
    <Card title="HTTPS">
      <div className="space-y-3 text-sm">
        <div className="flex flex-wrap items-center gap-2">
          <Badge tone="green">on</Badge>
          <span>{t.source === "provided" ? "using your certificate" : "using a certificate from this daemon's own certificate authority"}</span>
          {t.not_after && <span className="text-xs text-zinc-500">valid until {new Date(t.not_after * 1000).toLocaleDateString()}</span>}
        </div>
        <div className="text-xs text-zinc-500">Covers: {t.names.join(", ")}</div>
        {t.fingerprint_sha256 && (
          <div>
            <div className="text-xs font-medium text-zinc-600 dark:text-zinc-400">
              {t.source === "provided" ? "Certificate" : "Authority"} fingerprint (SHA-256) — compare before trusting:
            </div>
            <code className="break-all text-xs">{t.fingerprint_sha256}</code>
          </div>
        )}
        {t.ca_download && (
          <div className="space-y-2">
            <a className="inline-block rounded bg-zinc-900 px-2.5 py-1 text-xs font-medium text-white dark:bg-zinc-100 dark:text-zinc-900" href="/api/v1/tls/ca.pem">
              Download certificate authority
            </a>
            <p className="text-xs text-zinc-500">Trust it once per device; the daemon renews its server certificate from the same authority, so trust keeps working.</p>
            <div className="flex flex-wrap gap-1">
              {Object.keys(STEPS).map((k) => (
                <button
                  key={k}
                  className={`rounded px-2 py-0.5 text-xs ${k === os ? "bg-zinc-200 font-medium dark:bg-zinc-800" : "text-zinc-500 hover:bg-zinc-100 dark:hover:bg-zinc-900"}`}
                  onClick={() => setOs(k)}
                >
                  {k}
                </button>
              ))}
            </div>
            <ol className="list-decimal space-y-1 pl-5 text-xs">
              {STEPS[os].map((s) => (
                <li key={s}>{s}</li>
              ))}
            </ol>
          </div>
        )}
      </div>
    </Card>
  );
}
