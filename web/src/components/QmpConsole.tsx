import { useState } from "react";
import { useMutation } from "@tanstack/react-query";
import { api } from "../api/client";
import { useAuthState } from "./Auth";
import { Button, Card, Field, inputCls } from "./ui";

/** Raw QMP for experts (M6-08): only with expert mode on; every command is audited. */
export function QmpConsole({
  vmId,
  running,
}: {
  vmId: string;
  running: boolean;
}) {
  const expert = useAuthState().data?.expert_mode ?? false;
  const [cmd, setCmd] = useState("query-status");
  const [args, setArgs] = useState("");
  const [out, setOut] = useState<string | null>(null);
  const run = useMutation({
    mutationFn: () =>
      api.qmp(vmId, cmd.trim(), args.trim() ? JSON.parse(args) : undefined),
    onSuccess: (r) => setOut(JSON.stringify(r.return, null, 2)),
    onError: (e) => setOut(`error: ${(e as Error).message}`),
  });
  if (!expert) return null;
  return (
    <Card title="QMP console (expert)">
      <p className="mb-2 text-xs text-zinc-500">
        Commands go straight to QEMU and can break the VM or touch the host.
        Each one is recorded in the audit log.
      </p>
      <form
        className="flex flex-wrap items-end gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          run.mutate();
        }}
      >
        <Field label="Command">
          <input
            className={`${inputCls} font-mono`}
            value={cmd}
            onChange={(e) => setCmd(e.target.value)}
          />
        </Field>
        <Field label="Arguments (JSON, optional)">
          <input
            className={`${inputCls} w-72 font-mono`}
            value={args}
            onChange={(e) => setArgs(e.target.value)}
            placeholder='{"device": "qui-disk0"}'
          />
        </Field>
        <Button
          type="submit"
          disabled={!running || !cmd.trim() || run.isPending}
        >
          Run
        </Button>
        {!running && (
          <span className="text-xs text-zinc-500">start the VM first</span>
        )}
      </form>
      {out && (
        <pre className="mt-2 max-h-80 overflow-auto rounded bg-zinc-100 p-2 text-xs dark:bg-zinc-900">
          {out}
        </pre>
      )}
    </Card>
  );
}
