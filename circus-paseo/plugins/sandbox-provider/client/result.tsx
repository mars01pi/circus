import { useCallback, useMemo, useState } from "react";
import { Text, View } from "react-native";
import { useMutation } from "@tanstack/react-query";
import { useRpc, type PluginTimelineItemProps } from "@getpaseo/plugin/client";
import { copyText, SettingsAction } from "@getpaseo/plugin/client/ui";
import { readArtifact, retryCleanup, type TaskResultView } from "../shared/artifacts.js";
import { downloadFile } from "./web.js";

interface ArtifactAction {
  id: string;
  path: string;
  action: "preview" | "download" | "copy";
}
interface ArtifactRowProps {
  file: TaskResultView["artifacts"][number];
  pending: boolean;
  download: boolean;
  act(input: ArtifactAction): void;
}

export function SandboxResult({ item, theme, layout }: PluginTimelineItemProps<TaskResultView>) {
  const read = useRpc(readArtifact);
  const cleanup = useRpc(retryCleanup);
  const [preview, setPreview] = useState<string | null>(null);
  const clean = useMutation({ mutationFn: () => cleanup({ taskId: item.data.taskId }) });
  const artifact = useMutation({
    mutationFn: async (input: ArtifactAction) => {
      let offset = 0;
      const chunks: Uint8Array[] = [];
      while (true) {
        const chunk = await read({ id: input.id, offset });
        const binary = atob(chunk.base64);
        chunks.push(Uint8Array.from(binary, (char) => char.charCodeAt(0)));
        offset = chunk.nextOffset;
        if (chunk.complete) break;
      }
      const bytes = new Uint8Array(offset);
      let cursor = 0;
      for (const chunk of chunks) {
        bytes.set(chunk, cursor);
        cursor += chunk.length;
      }
      if (input.action === "download") {
        downloadFile(bytes, input.path);
        return "Download started.";
      }
      let text: string;
      try {
        text = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
      } catch {
        throw new Error("This is a binary file. Download it from the web or desktop app.");
      }
      if (input.action === "copy") {
        await copyText(text);
        return "File contents copied.";
      }
      setPreview(text.slice(0, 65536));
      return text.length > 65536
        ? "Preview limited to 64 KiB. Download for the full file."
        : "File preview loaded.";
    },
  });
  const cleanupState = clean.data?.cleanup ?? item.data.cleanup;
  const styles = useMemo(
    () => ({
      content: { gap: 8 },
      text: { color: theme.colors.foreground },
      muted: { color: theme.colors.foregroundMuted },
      error: { color: theme.colors.statusDanger },
    }),
    [theme],
  );
  const { mutate: retryMutation } = clean;
  const retry = useCallback(() => retryMutation(), [retryMutation]);
  return (
    <View style={styles.content}>
      <Text style={styles.text}>{item.data.message}</Text>
      <Text style={cleanupState === "confirmed" ? styles.muted : styles.error}>
        Sandbox cleanup: {cleanupState}
      </Text>
      {cleanupState === "pending" ? (
        <SettingsAction
          label="Retry sandbox cleanup"
          actionLabel={clean.isPending ? "Retrying…" : "Retry cleanup"}
          disabled={clean.isPending}
          onPress={retry}
        />
      ) : null}
      {cleanupState === "unknown" ? (
        <Text style={styles.error}>
          Check the runner and owned sandbox resources on the host. A lost connection does not
          confirm cleanup.
        </Text>
      ) : null}
      {clean.error ? (
        <Text accessibilityRole="alert" style={styles.error}>
          {clean.error.message}
        </Text>
      ) : null}
      {item.data.artifacts.map((file) => (
        <ArtifactRow
          key={file.id}
          file={file}
          pending={artifact.isPending}
          download={layout.platform === "web"}
          act={artifact.mutate}
        />
      ))}
      {artifact.isPending ? <Text style={styles.muted}>Reading exported file…</Text> : null}
      {artifact.error ? (
        <Text accessibilityRole="alert" style={styles.error}>
          {artifact.error.message}
        </Text>
      ) : null}
      {artifact.data ? <Text style={styles.muted}>{artifact.data}</Text> : null}
      {preview !== null ? (
        <Text selectable style={styles.text}>
          {preview}
        </Text>
      ) : null}
      <Text style={styles.muted}>This task has ended. Start a new sandbox task to run again.</Text>
    </View>
  );
}

function ArtifactRow({ file, pending, download, act }: ArtifactRowProps) {
  const preview = useCallback(
    () => act({ id: file.id, path: file.path, action: "preview" }),
    [file.id, file.path, act],
  );
  const save = useCallback(
    () => act({ id: file.id, path: file.path, action: download ? "download" : "copy" }),
    [file.id, file.path, download, act],
  );
  return (
    <View>
      <SettingsAction
        label={`${file.path} · ${file.size} bytes`}
        actionLabel="Preview"
        disabled={pending}
        onPress={preview}
      />
      <SettingsAction
        label={file.path}
        actionLabel={download ? "Download" : "Copy text"}
        disabled={pending}
        onPress={save}
      />
    </View>
  );
}
