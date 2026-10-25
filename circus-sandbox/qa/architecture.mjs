#!/usr/bin/env node
/**
 * Enforces circus-sandbox's fixed inter-domain dependency matrix from the
 * semantic `cargo modules dependencies` DOT graph. It never reads Rust source
 * and never uses cargo-modules' `--acyclic` flag.
 */

import { spawnSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import dotParser from "dotparser";

export const EXPECTED_TOOL_VERSION = "0.28.0";
export const FACADE = "facade";
export const ALLOWED_DEPENDENCIES = Object.freeze({
  error: [],
  observability: [],
  runtime: ["error"],
  infrastructure: ["runtime", "error", "observability"],
  environment: ["infrastructure", "runtime", "error"],
  workspace: ["infrastructure", "runtime", "error"],
  lifecycle: [
    "environment",
    "workspace",
    "infrastructure",
    "runtime",
    "error",
    "observability",
  ],
  pool: [
    "lifecycle",
    "environment",
    "workspace",
    "runtime",
    "error",
    "observability",
  ],
  http: ["api", "lifecycle", "workspace", "runtime", "error"],
  api: [
    "pool",
    "lifecycle",
    "environment",
    "workspace",
    "runtime",
    "error",
    "observability",
  ],
  facade: [
    "http",
    "api",
    "pool",
    "lifecycle",
    "environment",
    "workspace",
    "runtime",
    "error",
    "observability",
  ],
});

const DOMAINS = new Set(Object.keys(ALLOWED_DEPENDENCIES));
const ALLOWED = new Map(
  Object.entries(ALLOWED_DEPENDENCIES).map(([domain, targets]) => [
    domain,
    new Set(targets),
  ]),
);
const KINDS = new Set(
  "crate mod fn struct union enum variant const static trait type builtin macro".split(
    " ",
  ),
);
const MAX_OUTPUT = 20 * 1024 * 1024;
const TIMEOUT_MS = 10 * 60 * 1000;
const VERSION_PREFIX = "cargo-modules ";
const qaDir = path.dirname(fileURLToPath(import.meta.url));
const sandboxDir = path.resolve(qaDir, "..");

export class ArchitectureError extends Error {
  constructor(message) {
    super(message);
    this.name = "ArchitectureError";
  }
}

function bad(message) {
  throw new ArchitectureError(message);
}

function assertAcyclic() {
  const state = new Map();
  const visit = (domain) => {
    if (state.get(domain) === "done") return;
    if (state.get(domain) === "active") bad(`matrix cycles at ${domain}`);
    state.set(domain, "active");
    for (const target of ALLOWED_DEPENDENCIES[domain]) {
      if (!DOMAINS.has(target)) bad(`matrix names unknown domain ${target}`);
      visit(target);
    }
    state.set(domain, "done");
  };
  for (const domain of DOMAINS) visit(domain);
}

assertAcyclic();

function attributes(list) {
  const result = new Map();
  for (const attribute of list ?? []) {
    if (
      attribute?.type !== "attr" ||
      typeof attribute.id !== "string" ||
      (typeof attribute.eq !== "string" &&
        !(typeof attribute.eq === "number" && Number.isFinite(attribute.eq)))
    ) {
      bad("DOT attribute needs a string name and scalar value");
    }
    if (result.has(attribute.id))
      bad(`duplicate DOT attribute ${attribute.id}`);
    result.set(attribute.id, attribute.eq);
  }
  return result;
}

function parseLabel(value, id) {
  if (typeof value !== "string") bad(`node ${id} has no label`);
  const separator = value.indexOf("|");
  const header = separator > 0 ? value.slice(0, separator).trim() : "";
  const kind = header.split(/\s+/u).at(-1);
  if (!KINDS.has(kind)) bad(`node ${id} has label kind ${kind ?? "none"}`);
  return { kind, body: value.slice(separator + 1) };
}

function readGraph(dot) {
  let ast;
  try {
    ast = dotParser(dot);
  } catch (error) {
    bad(`cannot parse DOT: ${error.message}`);
  }
  if (!Array.isArray(ast) || ast.length !== 1) bad("DOT needs one graph");
  const [graph] = ast;
  if (graph?.type !== "digraph" || !Array.isArray(graph.children)) {
    bad("DOT must be a directed graph");
  }
  const nodes = new Map();
  const edges = [];
  const seen = new Set();
  for (const statement of graph.children) {
    if (statement?.type === "attr_stmt") {
      attributes(statement.attr_list);
      continue;
    }
    if (statement?.type === "node_stmt") {
      const id = statement.node_id?.id;
      if (typeof id !== "string" || id.length === 0)
        bad("DOT node id is empty");
      if (nodes.has(id)) bad(`duplicate DOT node ${id}`);
      const attrs = attributes(statement.attr_list);
      nodes.set(id, { id, ...parseLabel(attrs.get("label"), id) });
      continue;
    }
    if (statement?.type === "edge_stmt") {
      const targets = statement.edge_list;
      if (!Array.isArray(targets) || targets.length < 2) {
        bad("DOT edge needs endpoints");
      }
      for (const target of targets) {
        if (target?.type !== "node_id" || typeof target.id !== "string") {
          bad("DOT edge endpoint is not a node");
        }
      }
      if (attributes(statement.attr_list).get("label") !== "uses") {
        bad("DOT edge is not labelled uses");
      }
      for (let index = 0; index < targets.length - 1; index += 1) {
        const key = `${targets[index].id}\u0000${targets[index + 1].id}`;
        if (!seen.has(key)) {
          seen.add(key);
          edges.push([targets[index].id, targets[index + 1].id]);
        }
      }
      continue;
    }
    bad(`unsupported DOT statement ${statement?.type}`);
  }
  return { nodes, edges };
}

function checkNodes(nodes, crate) {
  let root;
  for (const node of nodes.values()) {
    if (node.id === crate) {
      if (node.kind !== "crate" || node.body !== crate)
        bad(`bad crate node ${crate}`);
      root = node;
      continue;
    }
    const prefix = `${crate}::`;
    if (!node.id.startsWith(prefix)) bad(`node ${node.id} is outside ${crate}`);
    if (node.kind === "crate") bad(`unexpected crate node ${node.id}`);
    const body = node.id.slice(prefix.length);
    if (body.split("::").some((segment) => segment.length === 0)) {
      bad(`node ${node.id} has an empty segment`);
    }
    if (node.body !== body) bad(`node ${node.id} label body is ${node.body}`);
  }
  if (!root) bad(`graph has no crate node ${crate}`);
}

function knownDomain(name, id) {
  if (DOMAINS.has(name)) return name;
  bad(`unknown top-level namespace ${name} on ${id}`);
}

function domainOf(node, nodes, crate) {
  if (node.id === crate) return FACADE;
  const segments = node.id.slice(crate.length + 2).split("::");
  const [first] = segments;
  const namespace = nodes.get(`${crate}::${first}`);
  if (segments.length === 1) {
    return node.kind === "mod" ? knownDomain(first, node.id) : FACADE;
  }
  if (namespace?.kind === "mod") return knownDomain(first, node.id);
  if (namespace) return FACADE;
  return knownDomain(first, node.id);
}

export function analyzeDot(dot, { crate = "circus_sandbox" } = {}) {
  if (!/^[A-Za-z_][A-Za-z0-9_]*$/u.test(crate))
    bad(`invalid crate name ${crate}`);
  const { nodes, edges } = readGraph(dot);
  checkNodes(nodes, crate);
  for (const [source, target] of edges) {
    if (!nodes.has(source) || !nodes.has(target)) {
      bad(`edge ${source} -> ${target} uses an undeclared node`);
    }
  }
  const domains = new Map(
    [...nodes.values()].map((node) => [node.id, domainOf(node, nodes, crate)]),
  );
  const violations = [];
  let intraDomainEdges = 0;
  for (const [source, target] of edges) {
    const from = domains.get(source);
    const to = domains.get(target);
    if (from === to) intraDomainEdges += 1;
    else if (!ALLOWED.get(from).has(to)) {
      violations.push({ source, target, from, to });
    }
  }
  return {
    crate,
    nodeCount: nodes.size,
    edgeCount: edges.length,
    domains: [...new Set(domains.values())].sort(),
    intraDomainEdges,
    crossDomainEdges: edges.length - intraDomainEdges,
    violations,
  };
}

function clip(value, limit) {
  const text = String(value).replace(/\s+/gu, " ").trim();
  return text.length <= limit ? text : `${text.slice(0, limit)}...`;
}

function run(command, args, cwd) {
  const result = spawnSync(command, args, {
    cwd,
    encoding: "utf8",
    timeout: TIMEOUT_MS,
    maxBuffer: MAX_OUTPUT,
    windowsHide: true,
    env: { ...process.env, NO_COLOR: "1" },
  });
  if (result.error) bad(`cannot run ${command}: ${result.error.message}`);
  if (result.status !== 0) {
    bad(
      `${command} ${args.join(" ")} failed (${result.status}): ${clip(result.stderr || result.stdout, 4000)}`,
    );
  }
  return result.stdout;
}

export function checkArchitecture({ cwd = sandboxDir } = {}) {
  const crate = "circus_sandbox";
  const output = run("cargo", ["modules", "--version"], cwd);
  const line = output
    .split(/\r?\n/u)
    .map((value) => value.trim())
    .find((value) => value.startsWith(VERSION_PREFIX));
  if (line?.slice(VERSION_PREFIX.length) !== EXPECTED_TOOL_VERSION) {
    bad(
      `cargo-modules ${EXPECTED_TOOL_VERSION} required; got ${clip(line ?? output, 200)}`,
    );
  }
  const dot = run(
    "cargo",
    [
      "modules",
      "dependencies",
      "--package",
      "circus-sandbox",
      "--lib",
      "--all-features",
      "--no-externs",
      "--no-owns",
    ],
    cwd,
  );
  return analyzeDot(dot, { crate });
}

function main() {
  try {
    const result = checkArchitecture();
    if (result.violations.length === 0) {
      console.log(
        `architecture: ${result.nodeCount} nodes, ${result.crossDomainEdges} cross-domain and ${result.intraDomainEdges} intra-domain uses edges; matrix satisfied`,
      );
      return;
    }
    console.error(
      `architecture: ${result.violations.length} forbidden cross-domain uses in ${result.crate}`,
    );
    for (const violation of result.violations.slice(0, 25)) {
      const allowed = ALLOWED_DEPENDENCIES[violation.from].join(", ");
      console.error(
        `${violation.source} -> ${violation.target}: ${violation.from} may only depend on ${allowed || "nothing"}`,
      );
    }
    if (result.violations.length > 25) {
      console.error(`... ${result.violations.length - 25} more`);
    }
    process.exitCode = 1;
  } catch (error) {
    console.error(`architecture: ${error.message}`);
    process.exitCode = 1;
  }
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(process.argv[1]).href
) {
  main();
}
