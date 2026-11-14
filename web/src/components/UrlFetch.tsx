import { useState } from "react";
import { useMutation } from "@tanstack/react-query";
import { api } from "../api/client";
import { Button, Card, Field, inputCls } from "./ui";

/** File name the daemon would derive from the URL's last path segment. */
function nameFromUrl(url: string): string {
  try {
    const segs = new URL(url).pathname.split("/").filter(Boolean);
    return segs.length ? decodeURIComponent(segs[segs.length - 1]) : "";
  } catch {
    return "";
  }
}

/** Download an image from a URL into a pool (M2-08). */
export function UrlFetch({ pool }: { pool: string }) {
  const [url, setUrl] = useState("");
  const [name, setName] = useState("");
  const [nameEdited, setNameEdited] = useState(false);
  const [sha256, setSha256] = useState("");
  const fetchJob = useMutation({
    mutationFn: () => api.fetchUrl(pool, url.trim(), name.trim() || undefined, sha256.trim() || undefined),
    onSuccess: () => {
      setUrl("");
      setName("");
      setNameEdited(false);
      setSha256("");
    },
  });
  const insecure = url.trim().toLowerCase().startsWith("http://");
  return (
    <Card title={`Download from URL into ${pool}`}>
      <form
        className="space-y-3"
        onSubmit={(e) => {
          e.preventDefault();
          if (url.trim()) fetchJob.mutate();
        }}
      >
        <Field label="URL">
          <input
            className={`${inputCls} font-mono`}
            value={url}
            placeholder="https://cdimage.example.org/release/installer.iso"
            onChange={(e) => {
              setUrl(e.target.value);
              if (!nameEdited) setName(nameFromUrl(e.target.value));
            }}
          />
        </Field>
        <div className="grid grid-cols-2 gap-3">
          <Field label="Save as">
            <input
              className={inputCls}
              value={name}
              onChange={(e) => {
                setName(e.target.value);
                setNameEdited(true);
              }}
            />
          </Field>
          <Field label={insecure ? "SHA-256 (required for http)" : "SHA-256 (optional)"}>
            <input className={`${inputCls} font-mono`} value={sha256} onChange={(e) => setSha256(e.target.value)} />
          </Field>
        </div>
        {insecure && (
          <p className="text-xs text-amber-700 dark:text-amber-400">
            Plain http is not encrypted and can be tampered with on the way. It is only accepted with a SHA-256 checksum
            from a trusted source, which the download is verified against.
          </p>
        )}
        <div className="flex items-center gap-2">
          <Button type="submit" disabled={!url.trim() || fetchJob.isPending}>
            Start download
          </Button>
          {fetchJob.isSuccess && <span className="text-xs text-zinc-500">Started — see Jobs.</span>}
        </div>
        {fetchJob.isError && <p className="text-xs text-rose-600">{fetchJob.error.message}</p>}
        <p className="text-xs text-zinc-500">
          Downloads only reach public internet addresses: never this computer, the local network or cloud metadata
          services. Redirects are checked the same way.
        </p>
      </form>
    </Card>
  );
}
