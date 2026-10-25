import { useCallback, useReducer } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Text, View } from "react-native";
import { StyleSheet } from "react-native-unistyles";
import {
  sandboxSetupResultSchema,
  sandboxSetupSnapshotSchema,
  type SandboxResourcePreset,
  type SandboxSetupInput,
  type SandboxSetupSnapshot,
} from "@getpaseo/protocol/sandbox-setup";
import { Button } from "@/components/ui/button";
import { Alert } from "@/components/ui/alert";
import { EditingTextInput } from "@/components/ui/text-input";
import { StatusBadge } from "@/components/ui/status-badge";
import { SettingsSection } from "@/components/settings/headings/settings-section";
import { getIsElectron } from "@/constants/platform";
import { invokeDesktopCommand } from "@/desktop/electron/invoke";
import { useFetchQuery } from "@/data/query";
import { settingsStyles } from "@/styles/settings";
import { GithubConnection } from "./github-connection";

const queryKey = ["desktop-sandbox-setup"];
type Action = "prepare" | "test" | "save" | "select_source";

interface Draft extends SandboxSetupInput {
  secretRevision: number;
}
type DraftChange =
  | { type: "field"; field: "baseUrl" | "model" | "apiKey"; value: string }
  | { type: "preset"; value: SandboxResourcePreset }
  | { type: "clear-secret" };

function reduceDraft(state: Draft, change: DraftChange): Draft {
  if (change.type === "field") return { ...state, [change.field]: change.value };
  if (change.type === "preset") return { ...state, preset: change.value };
  return { ...state, apiKey: "", secretRevision: state.secretRevision + 1 };
}

export function SandboxSetupSection() {
  const query = useFetchQuery({
    dataShape: "value",
    staleTimeMs: 30_000,
    queryKey,
    enabled: getIsElectron(),
    queryFn: async () =>
      sandboxSetupSnapshotSchema.parse(await invokeDesktopCommand("sandbox_setup_inspect")),
    retry: false,
  });
  const refresh = useCallback(() => {
    void query.refetch();
  }, [query]);
  if (!getIsElectron()) return null;
  if (query.isPending)
    return (
      <SettingsSection title="Circus sandbox">
        <Text style={styles.muted}>Checking this computer…</Text>
      </SettingsSection>
    );
  if (query.isError)
    return (
      <SettingsSection title="Circus sandbox">
        <Alert
          variant="error"
          title="Could not check sandbox setup"
          description="Check the local host configuration and try again."
        />
        <Button onPress={refresh}>Check again</Button>
      </SettingsSection>
    );
  return <SetupForm snapshot={query.data} refresh={refresh} refreshing={query.isFetching} />;
}

interface SetupFormProps {
  snapshot: SandboxSetupSnapshot;
  refresh(): void;
  refreshing: boolean;
}

function SetupForm({ snapshot, refresh, refreshing }: SetupFormProps) {
  const queryClient = useQueryClient();
  const [draft, dispatch] = useReducer(reduceDraft, {
    baseUrl: snapshot.settings.baseUrl,
    model: snapshot.settings.model,
    apiKey: "",
    preset: snapshot.settings.preset,
    secretRevision: 0,
  });
  const action = useMutation({
    // Only the action name enters React Query's mutation variables; never cache the API key.
    mutationFn: async (operation: Action) => {
      if (operation === "select_source") {
        const selected = sandboxSetupSnapshotSchema.parse(
          await invokeDesktopCommand("sandbox_setup_select_source"),
        );
        queryClient.setQueryData(queryKey, selected);
        return { ok: true, message: "Environment checked." };
      }
      const input = {
        baseUrl: draft.baseUrl,
        model: draft.model,
        apiKey: draft.apiKey,
        preset: draft.preset,
      };
      return sandboxSetupResultSchema.parse(
        await invokeDesktopCommand(
          `sandbox_setup_${operation}`,
          operation === "prepare" ? undefined : input,
        ),
      );
    },
    onSettled: (_result, _error, operation) => {
      if (operation === "save") dispatch({ type: "clear-secret" });
      if (operation !== "test") void queryClient.invalidateQueries({ queryKey });
    },
  });
  const setBaseUrl = useCallback(
    (value: string) => dispatch({ type: "field", field: "baseUrl", value }),
    [],
  );
  const setModel = useCallback(
    (value: string) => dispatch({ type: "field", field: "model", value }),
    [],
  );
  const setApiKey = useCallback(
    (value: string) => dispatch({ type: "field", field: "apiKey", value }),
    [],
  );
  const prepare = useCallback(() => action.mutate("prepare"), [action]);
  const selectSource = useCallback(() => action.mutate("select_source"), [action]);
  const test = useCallback(() => action.mutate("test"), [action]);
  const save = useCallback(() => action.mutate("save"), [action]);
  const busy = action.isPending || refreshing;
  const configured = snapshot.settings.hasCredential;
  const hasModel = Boolean(draft.baseUrl.trim() && draft.model.trim());
  const canConnect = snapshot.supported && hasModel && !busy;
  const prepareLabel =
    action.isPending && action.variables === "prepare"
      ? "Preparing… this can take several minutes"
      : "Prepare runtime";
  return (
    <SettingsSection
      title="Circus sandbox"
      info="Run one Codex task in a disposable environment. Only this computer's desktop app can change these settings; remote task clients cannot manage credentials."
    >
      <View style={[settingsStyles.card, styles.content]}>
        <View style={styles.heading}>
          <Text style={styles.title}>A workspace for your agent. Your keys stay here.</Text>
          <Text style={styles.muted}>
            Connect your model service once. Import only the files you want the agent to use, then
            bring the results back.
          </Text>
        </View>
        <View style={styles.group}>
          <Text style={styles.label}>1. Check this computer</Text>
          {snapshot.checks.map((check) => (
            <EnvironmentRow key={check.id} check={check} />
          ))}
          <View style={styles.actions}>
            <Button variant="outline" size="sm" onPress={refresh} disabled={busy}>
              Check again
            </Button>
            <Button
              variant="outline"
              size="sm"
              onPress={selectSource}
              disabled={busy || !snapshot.supported}
            >
              Choose Circus folder
            </Button>
            <Button size="sm" onPress={prepare} disabled={busy || !snapshot.supported}>
              {prepareLabel}
            </Button>
          </View>
        </View>
        <View style={styles.group}>
          <Text style={styles.label}>2. Connect your model</Text>
          <Text style={styles.muted}>
            Codex · OpenAI-compatible Responses API. Use a model service you trust; task contents
            are sent to that service.
          </Text>
          <Text style={styles.fieldLabel}>Service URL</Text>
          <EditingTextInput
            accessibilityLabel="Sandbox model service URL"
            initialValue={draft.baseUrl}
            onChangeText={setBaseUrl}
            autoCapitalize="none"
            autoCorrect={false}
            placeholder="https://your-service.example/v1"
            style={styles.input}
            editable={!busy && snapshot.supported}
          />
          <Text style={styles.fieldLabel}>Model name</Text>
          <EditingTextInput
            accessibilityLabel="Sandbox model name"
            initialValue={draft.model}
            onChangeText={setModel}
            autoCapitalize="none"
            autoCorrect={false}
            placeholder="Model enabled by your service"
            style={styles.input}
            editable={!busy && snapshot.supported}
          />
          <Text style={styles.fieldLabel}>API key</Text>
          <EditingTextInput
            key={draft.secretRevision}
            accessibilityLabel="Sandbox model API key"
            initialValue=""
            onChangeText={setApiKey}
            autoCapitalize="none"
            autoCorrect={false}
            secureTextEntry
            autoComplete="off"
            placeholder={
              configured
                ? "Saved on this computer · leave blank to keep"
                : "Stored in macOS Keychain"
            }
            style={styles.input}
            editable={!busy && snapshot.supported}
          />
          {snapshot.settings.credentialStorage === "file" ? (
            <Text style={styles.muted}>
              Your existing host credential file is preserved. Saving creates a Keychain-backed
              configuration.
            </Text>
          ) : null}
          <Text style={styles.muted}>
            Your key is never included in sandbox files, environment variables, or task messages.
            Changing the service URL requires entering a key again.
          </Text>
          <Button variant="outline" size="sm" onPress={test} disabled={!canConnect}>
            Test model connection
          </Button>
          <Text style={styles.muted}>
            Sends a small request to your model service; provider usage charges may apply.
          </Text>
        </View>
        <View style={styles.group}>
          <Text style={styles.label}>3. Choose a resource budget</Text>
          <ResourceChoices
            preset={draft.preset}
            hasCustom={snapshot.settings.preset === "custom"}
            busy={busy}
            dispatch={dispatch}
          />
          <Text style={styles.muted}>
            Each task starts with an empty workspace. You choose its input files, output files and
            deadline. Host folders and personal Agent settings are not imported.
          </Text>
          <Alert
            variant="info"
            title="Default permissions"
            description="The sandbox has no direct network access. Model requests pass through the host connector. Tool connections require separate setup below and explicit permission for each task."
          />
        </View>
        {action.isError ? (
          <Alert
            variant="error"
            title="Setup was not completed"
            description="Check the environment and entered values. If you changed the service URL, enter its API key. Existing settings remain available; retry when the issue is resolved."
          />
        ) : null}
        {action.data ? (
          <Alert variant={action.data.ok ? "success" : "error"} description={action.data.message} />
        ) : null}
        <Button onPress={save} disabled={!snapshot.ready || !canConnect}>
          Save and enable sandbox
        </Button>
        <GithubConnection enabled={snapshot.supported && configured && !busy} />
      </View>
    </SettingsSection>
  );
}

interface ResourceChoicesProps {
  preset: SandboxResourcePreset;
  hasCustom: boolean;
  busy: boolean;
  dispatch(change: DraftChange): void;
}
function ResourceChoices({ preset, hasCustom, busy, dispatch }: ResourceChoicesProps) {
  const compact = useCallback(() => dispatch({ type: "preset", value: "compact" }), [dispatch]);
  const balanced = useCallback(() => dispatch({ type: "preset", value: "balanced" }), [dispatch]);
  const custom = useCallback(() => dispatch({ type: "preset", value: "custom" }), [dispatch]);
  const compactVariant = preset === "compact" ? "default" : "outline";
  const balancedVariant = preset === "balanced" ? "default" : "outline";
  const customVariant = preset === "custom" ? "default" : "outline";
  return (
    <View style={styles.actions}>
      <Button variant={compactVariant} size="sm" onPress={compact} disabled={busy}>
        Light · 1 CPU / 512 MB
      </Button>
      <Button variant={balancedVariant} size="sm" onPress={balanced} disabled={busy}>
        Standard · 2 CPUs / 1 GB
      </Button>
      {hasCustom ? (
        <Button variant={customVariant} size="sm" onPress={custom} disabled={busy}>
          Keep existing budget
        </Button>
      ) : null}
    </View>
  );
}

function EnvironmentRow({ check }: { check: SandboxSetupSnapshot["checks"][number] }) {
  const label = check.ready ? "Ready" : "Needs attention";
  const variant = check.ready ? "success" : "warning";
  return (
    <View style={styles.check}>
      <View style={styles.checkText}>
        <Text style={styles.fieldLabel}>{check.label}</Text>
        <Text style={styles.muted}>{check.detail}</Text>
      </View>
      <StatusBadge label={label} variant={variant} />
    </View>
  );
}

const styles = StyleSheet.create((theme) => ({
  content: { padding: theme.spacing[4], gap: theme.spacing[6] },
  heading: { gap: theme.spacing[2] },
  title: {
    fontSize: theme.fontSize.lg,
    fontWeight: theme.fontWeight.semibold,
    color: theme.colors.foreground,
  },
  label: {
    fontSize: theme.fontSize.base,
    fontWeight: theme.fontWeight.semibold,
    color: theme.colors.foreground,
  },
  fieldLabel: { fontSize: theme.fontSize.sm, color: theme.colors.foreground },
  muted: { color: theme.colors.foregroundMuted, fontSize: theme.fontSize.sm, lineHeight: 20 },
  group: { gap: theme.spacing[3] },
  actions: { flexDirection: "row", flexWrap: "wrap", gap: theme.spacing[2] },
  check: {
    flexDirection: "row",
    flexWrap: "wrap",
    alignItems: "flex-start",
    gap: theme.spacing[2],
  },
  checkText: { flex: 1, minWidth: 180, gap: theme.spacing[1] },
  input: {
    borderWidth: 1,
    borderColor: theme.colors.border,
    borderRadius: theme.borderRadius.md,
    padding: theme.spacing[3],
    color: theme.colors.foreground,
    fontSize: theme.fontSize.base,
  },
}));
