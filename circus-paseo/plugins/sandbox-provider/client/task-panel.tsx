import { useCallback, useEffect, useMemo, useState, useSyncExternalStore } from "react";
import { Text } from "react-native";
import { usePaseo, useTabState, type PluginWorkspacePanelProps } from "@getpaseo/plugin/client";
import { ScrollView, SettingsAction, SettingsSection, TextInput } from "@getpaseo/plugin/client/ui";
import {
  SANDBOX_PROVIDER_SELECTION,
  SANDBOX_SESSION_NOTICE,
  inputFileBytes,
  type InputFile,
} from "../shared/task.js";
import { openTaskForm, taskFormDraftSchema, type TaskFormDraft } from "./task-form.js";
import { chooseFiles } from "./web.js";
import { ToolPermissions } from "./tool-permissions.js";

const defaultDraft: TaskFormDraft = {
  prompt: "",
  outputPaths: "",
  timeoutSeconds: "300",
  filePath: "",
  fileText: "",
  files: [],
  mcpGrants: [],
};

export function TaskPanel({
  workspaceId,
  host,
  theme,
  layout,
  navigation,
  tab,
}: PluginWorkspacePanelProps) {
  const paseo = usePaseo();
  const [draft, setDraft] = useTabState(tab, taskFormDraftSchema, defaultDraft);
  const [form] = useState(() =>
    openTaskForm({
      initial: draft,
      onDraftChange: setDraft,
      async create(input) {
        const agent = await paseo.workspaces.ref(workspaceId).agents.create({
          config: { provider: SANDBOX_PROVIDER_SELECTION, options: input.options },
          prompt: input.prompt,
          title: "Sandbox task",
        });
        return agent.id;
      },
    }),
  );
  useEffect(() => () => form.close(), [form]);
  const state = useSyncExternalStore(form.subscribe, form.getState);
  const pending = state.submission.status === "submitting";
  const styles = useMemo(
    () => ({
      content: { padding: 16, gap: 16 },
      muted: { color: theme.colors.foregroundMuted },
      error: { color: theme.colors.statusDanger },
      success: { color: theme.colors.statusSuccess },
      input: {
        color: theme.colors.foreground,
        borderColor: theme.colors.border,
        borderWidth: 1,
        borderRadius: 6,
        padding: 10,
        minHeight: 80,
      },
      compactInput: {
        color: theme.colors.foreground,
        borderColor: theme.colors.border,
        borderWidth: 1,
        borderRadius: 6,
        padding: 10,
      },
    }),
    [theme],
  );
  const controls = useMemo(
    () => ({
      prompt: (text: string) => form.set("prompt", text),
      timeout: (text: string) => form.set("timeoutSeconds", text),
      path: (text: string) => form.set("filePath", text),
      contents: (text: string) => form.set("fileText", text),
      outputs: (text: string) => form.set("outputPaths", text),
      add: () => form.addTextFile(),
      submit: () => {
        void form.submit();
      },
    }),
    [form],
  );
  const importFiles = useCallback(async () => {
    try {
      form.addFiles(await chooseFiles());
    } catch (error) {
      form.fail(error instanceof Error ? error.message : "Could not import files.");
    }
  }, [form]);
  const created = state.submission.status === "created" ? state.submission.agentId : null;
  const openCreated = useCallback(() => {
    if (created && navigation)
      navigation.openAgent({ agentId: created, serverId: host.id, workspaceId });
  }, [created, host.id, navigation, workspaceId]);
  return (
    <ScrollView contentContainerStyle={styles.content}>
      <Text style={styles.muted}>{SANDBOX_SESSION_NOTICE}</Text>
      <SettingsSection title="Task">
        <TextInput
          accessibilityLabel="Sandbox prompt"
          placeholder="What should the agent do?"
          multiline
          editable={!pending}
          value={state.prompt}
          onChangeText={controls.prompt}
          style={styles.input}
        />
        <Text style={styles.muted}>Deadline (seconds)</Text>
        <TextInput
          accessibilityLabel="Deadline (seconds)"
          value={state.timeoutSeconds}
          keyboardType="number-pad"
          editable={!pending}
          onChangeText={controls.timeout}
          style={styles.compactInput}
        />
      </SettingsSection>
      <SettingsSection title="Input files">
        {layout.platform === "web" ? (
          <SettingsAction
            label="Choose files from this device"
            actionLabel="Choose files"
            disabled={pending}
            onPress={importFiles}
          />
        ) : null}
        <Text style={styles.muted}>Relative file path</Text>
        <TextInput
          accessibilityLabel="Relative file path"
          value={state.filePath}
          placeholder="src/answer.py"
          editable={!pending}
          onChangeText={controls.path}
          style={styles.compactInput}
        />
        <TextInput
          accessibilityLabel="Input file contents"
          value={state.fileText}
          placeholder="Paste file contents"
          multiline
          editable={!pending}
          onChangeText={controls.contents}
          style={styles.input}
        />
        <SettingsAction
          label="Add pasted file"
          actionLabel="Add file"
          disabled={pending}
          onPress={controls.add}
        />
        {state.files.map((file) => (
          <InputFileRow key={file.path} file={file} pending={pending} remove={form.removeFile} />
        ))}
      </SettingsSection>
      <SettingsSection title="Files to export">
        <TextInput
          accessibilityLabel="Output file paths"
          value={state.outputPaths}
          placeholder={"One relative file path per line"}
          multiline
          editable={!pending}
          onChangeText={controls.outputs}
          style={styles.input}
        />
      </SettingsSection>
      <SettingsSection title="Tool permissions">
        <ToolPermissions
          hostId={host.id}
          theme={theme}
          grants={state.mcpGrants}
          disabled={pending}
          onChange={form.setMcpGrants}
        />
      </SettingsSection>
      {state.submission.status === "failed" ? (
        <Text accessibilityRole="alert" style={styles.error}>
          {state.submission.message}
        </Text>
      ) : null}
      {created ? <Text style={styles.success}>Sandbox task created.</Text> : null}
      {created && navigation ? (
        <SettingsAction label="Task output" actionLabel="Open task" onPress={openCreated} />
      ) : null}
      <SettingsAction
        label="Start a new sandbox"
        actionLabel={pending ? "Starting…" : "Start task"}
        disabled={pending}
        onPress={controls.submit}
      />
    </ScrollView>
  );
}

interface InputFileRowProps {
  file: InputFile;
  pending: boolean;
  remove(path: string): void;
}
function InputFileRow({ file, pending, remove }: InputFileRowProps) {
  const onRemove = useCallback(() => remove(file.path), [file.path, remove]);
  return (
    <SettingsAction
      label={`${file.path} · ${inputFileBytes(file)} bytes`}
      actionLabel="Remove"
      disabled={pending}
      onPress={onRemove}
    />
  );
}
