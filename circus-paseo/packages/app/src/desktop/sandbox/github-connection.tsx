import { useCallback, useReducer } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Text, View } from "react-native";
import { StyleSheet } from "react-native-unistyles";
import {
  sandboxSetupResultSchema,
  type SandboxGithubInput,
} from "@getpaseo/protocol/sandbox-setup";
import { Button } from "@/components/ui/button";
import { Alert } from "@/components/ui/alert";
import { EditingTextInput } from "@/components/ui/text-input";
import { invokeDesktopCommand } from "@/desktop/electron/invoke";

interface Draft extends SandboxGithubInput {
  secretRevision: number;
}
type Change = { field: keyof SandboxGithubInput; value: string } | { clearSecret: true };
function reducer(draft: Draft, change: Change): Draft {
  if ("clearSecret" in change)
    return { ...draft, token: "", secretRevision: draft.secretRevision + 1 };
  return { ...draft, [change.field]: change.value };
}

export function GithubConnection({ enabled }: { enabled: boolean }) {
  const client = useQueryClient();
  const [draft, dispatch] = useReducer(reducer, {
    id: "github",
    url: "https://api.githubcopilot.com/mcp/",
    token: "",
    owner: "",
    repo: "",
    secretRevision: 0,
  });
  const save = useMutation({
    mutationFn: async () => {
      const { secretRevision: _revision, ...input } = draft;
      return sandboxSetupResultSchema.parse(
        await invokeDesktopCommand("sandbox_setup_mcp_save", input),
      );
    },
    onSettled: () => dispatch({ clearSecret: true }),
    onSuccess: () => {
      void client.invalidateQueries({ queryKey: ["sandbox-mcp-connections"] });
    },
  });
  const connect = useCallback(() => save.mutate(), [save]);
  const name = useCallback((value: string) => dispatch({ field: "id", value }), []);
  const endpoint = useCallback((value: string) => dispatch({ field: "url", value }), []);
  const owner = useCallback((value: string) => dispatch({ field: "owner", value }), []);
  const repo = useCallback((value: string) => dispatch({ field: "repo", value }), []);
  const token = useCallback((value: string) => dispatch({ field: "token", value }), []);
  const editable = enabled && !save.isPending;
  const complete = Boolean(draft.id && draft.url && draft.owner && draft.repo && draft.token);
  return (
    <View style={styles.group}>
      <Text style={styles.label}>4. Connect a tool · GitHub issues</Text>
      <Text style={styles.muted}>
        Optional. This preset connects to a GitHub-compatible Streamable HTTP MCP service. It is
        verified before saving; no connection is assumed from the preset alone.
      </Text>
      {!enabled ? (
        <Text style={styles.muted}>Save your sandbox setup first to connect a service.</Text>
      ) : null}
      <Text style={styles.field}>Connection name</Text>
      <EditingTextInput
        accessibilityLabel="MCP connection name"
        initialValue={draft.id}
        onChangeText={name}
        editable={editable}
        autoCapitalize="none"
        autoCorrect={false}
        style={styles.input}
      />
      <Text style={styles.field}>MCP service URL</Text>
      <EditingTextInput
        accessibilityLabel="MCP service URL"
        initialValue={draft.url}
        onChangeText={endpoint}
        editable={editable}
        autoCapitalize="none"
        autoCorrect={false}
        style={styles.input}
      />
      <Text style={styles.field}>Repository owner</Text>
      <EditingTextInput
        accessibilityLabel="GitHub repository owner"
        initialValue=""
        onChangeText={owner}
        placeholder="your-team"
        editable={editable}
        autoCapitalize="none"
        autoCorrect={false}
        style={styles.input}
      />
      <Text style={styles.field}>Repository name</Text>
      <EditingTextInput
        accessibilityLabel="GitHub repository name"
        initialValue=""
        onChangeText={repo}
        placeholder="your-project"
        editable={editable}
        autoCapitalize="none"
        autoCorrect={false}
        style={styles.input}
      />
      <Text style={styles.field}>Access token</Text>
      <EditingTextInput
        key={draft.secretRevision}
        accessibilityLabel="GitHub access token"
        initialValue=""
        onChangeText={token}
        placeholder="Stored in macOS Keychain"
        secureTextEntry
        autoComplete="off"
        autoCapitalize="none"
        autoCorrect={false}
        editable={editable}
        style={styles.input}
      />
      <Text style={styles.muted}>
        Use a token restricted to the selected repository with read access to issues. The connection
        check lists tools; it does not read or change repository data.
      </Text>
      <Alert
        variant="info"
        title="Permission you are adding"
        description="Read a single issue in this repository. The host fixes the owner, repository and read operation. The agent can choose only the issue number. Every new task must explicitly allow this tool."
      />
      {save.data ? (
        <Alert variant={save.data.ok ? "success" : "error"} description={save.data.message} />
      ) : null}
      {save.isError ? (
        <Alert
          variant="error"
          title="Could not connect GitHub"
          description="Check the local host, HTTPS endpoint, repository and token, then retry."
        />
      ) : null}
      <Button variant="outline" onPress={connect} disabled={!editable || !complete}>
        {save.isPending ? "Checking service and saving…" : "Verify and connect GitHub"}
      </Button>
    </View>
  );
}

const styles = StyleSheet.create((theme) => ({
  group: { gap: theme.spacing[3] },
  label: {
    fontSize: theme.fontSize.base,
    fontWeight: theme.fontWeight.semibold,
    color: theme.colors.foreground,
  },
  field: { fontSize: theme.fontSize.sm, color: theme.colors.foreground },
  muted: { color: theme.colors.foregroundMuted, fontSize: theme.fontSize.sm, lineHeight: 20 },
  input: {
    borderWidth: 1,
    borderColor: theme.colors.border,
    borderRadius: theme.borderRadius.md,
    padding: theme.spacing[3],
    color: theme.colors.foreground,
    fontSize: theme.fontSize.base,
  },
}));
