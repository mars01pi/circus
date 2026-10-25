import { useCallback, useMemo } from "react";
import { Text, View } from "react-native";
import { useQuery } from "@tanstack/react-query";
import { useRpc, type PluginWorkspacePanelProps } from "@getpaseo/plugin/client";
import { SettingsAction, SettingsSwitch } from "@getpaseo/plugin/client/ui";
import { listMcpConnections, type McpConnectionView, type McpGrant } from "../shared/mcp.js";

const contentStyle = { gap: 8 };

interface ToolPermissionsProps {
  hostId: string;
  theme: PluginWorkspacePanelProps["theme"];
  grants: McpGrant[];
  disabled: boolean;
  onChange(grants: McpGrant[]): void;
}

export function ToolPermissions({
  hostId,
  theme,
  grants,
  disabled,
  onChange,
}: ToolPermissionsProps) {
  const list = useRpc(listMcpConnections);
  const query = useQuery({
    queryKey: ["sandbox-mcp-connections", hostId],
    queryFn: () => list({}),
    retry: false,
  });
  const refresh = useCallback(() => {
    void query.refetch();
  }, [query]);
  const clear = useCallback(() => onChange([]), [onChange]);
  const muted = useMemo(() => ({ color: theme.colors.foregroundMuted }), [theme]);
  const connections = query.data?.status === "ready" ? query.data.connections : [];
  return (
    <View style={contentStyle}>
      <Text style={muted}>
        Choose tools for this task. Nothing is authorized by default. Credentials stay on the host;
        fixed resources are enforced there.
      </Text>
      {query.isPending ? <Text style={muted}>Loading configured tools…</Text> : null}
      {query.isError || query.data?.status === "unavailable" ? (
        <Text style={muted}>
          Tool configuration is unavailable. You can run a task without MCP, or configure services
          in the local desktop app.
        </Text>
      ) : null}
      {query.data?.status === "ready" && connections.length === 0 ? (
        <Text style={muted}>
          No MCP services configured. Add a service in the local desktop host Providers settings.
        </Text>
      ) : null}
      {connections.map((connection) => (
        <ConnectionTools
          key={connection.id}
          connection={connection}
          grants={grants}
          onChange={onChange}
          disabled={disabled}
        />
      ))}
      {connections.length > 0 ? (
        <Text style={muted}>
          These are configured permissions. Availability and authentication are checked when a task
          connects.
        </Text>
      ) : null}
      <SettingsAction
        label="Configured tools"
        actionLabel="Refresh"
        onPress={refresh}
        disabled={disabled || query.isFetching}
      />
      {grants.length > 0 ? (
        <SettingsAction
          label={`${grants.length} service(s) allowed for this task`}
          actionLabel="Clear permissions"
          onPress={clear}
          disabled={disabled}
        />
      ) : null}
    </View>
  );
}

interface ConnectionToolsProps {
  connection: McpConnectionView;
  grants: McpGrant[];
  disabled: boolean;
  onChange(grants: McpGrant[]): void;
}
function ConnectionTools({ connection, grants, disabled, onChange }: ConnectionToolsProps) {
  return (
    <View>
      {connection.tools.map((tool) => (
        <ToolPermission
          key={tool.name}
          connectionId={connection.id}
          tool={tool}
          grants={grants}
          disabled={disabled}
          onChange={onChange}
        />
      ))}
    </View>
  );
}

interface ToolPermissionProps extends Omit<ConnectionToolsProps, "connection"> {
  connectionId: string;
  tool: McpConnectionView["tools"][number];
}
function ToolPermission({ connectionId, tool, grants, disabled, onChange }: ToolPermissionProps) {
  const grant = grants.find((candidate) => candidate.connectionId === connectionId);
  const checked = grant?.tools.includes(tool.name) ?? false;
  const change = useCallback(
    (enabled: boolean) => {
      const otherGrants = grants.filter((candidate) => candidate.connectionId !== connectionId);
      const otherTools = grant?.tools.filter((name) => name !== tool.name) ?? [];
      const tools = enabled ? [...otherTools, tool.name] : otherTools;
      onChange(tools.length > 0 ? [...otherGrants, { connectionId, tools }] : otherGrants);
    },
    [connectionId, grant, grants, onChange, tool.name],
  );
  const resources = Object.entries(tool.fixedArguments)
    .map(([name, value]) => `${name}: ${value}`)
    .join(" · ");
  return (
    <SettingsSwitch
      label={`${connectionId} / ${tool.name}`}
      hint={resources || "No fixed resource restriction configured"}
      value={checked}
      disabled={disabled}
      onValueChange={change}
    />
  );
}
