#!/usr/bin/env python3
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def patch(path: str, old: str, new: str, count: int = 1):
    p = ROOT / path
    text = p.read_text(encoding="utf-8")
    found = text.count(old)
    if found != count:
        raise RuntimeError(f"{path}: expected {count} exact matches, found {found}: {old[:100]!r}")
    p.write_text(text.replace(old, new, count), encoding="utf-8")


# Authenticated dashboard API routes. NodeTransport itself is a separate
# listener/namespace and is not added here.
patch(
    "recisdb-proxy/src/web/mod.rs",
    '''        .route("/version", get(api::get_version))\n''',
    '''        .route("/version", get(api::get_version))\n        // Distributed-node configuration and active path tests. These are\n        // dashboard operations and therefore inherit the normal /api auth.\n        .route("/nodes", get(api::get_nodes).post(api::upsert_node))\n        .route("/nodes/:id/probe", post(api::probe_node))\n        .route("/node-route-groups/member", post(api::set_route_group_member))\n''',
)

# Vue dashboard tab.
patch(
    "web-ui/src/App.vue",
    "import SettingsView from './components/SettingsView.vue'\n",
    "import SettingsView from './components/SettingsView.vue'\nimport NodesView from './components/NodesView.vue'\n",
)
patch(
    "web-ui/src/App.vue",
    "  { id: 'channels', label: 'チャンネル', icon: '⌁' },\n",
    "  { id: 'channels', label: 'チャンネル', icon: '⌁' },\n  { id: 'nodes', label: '分散ノード', icon: '◇' },\n",
)
patch(
    "web-ui/src/App.vue",
    "        <ChannelsView v-else-if=\"active === 'channels'\" />\n",
    "        <ChannelsView v-else-if=\"active === 'channels'\" />\n        <NodesView v-else-if=\"active === 'nodes'\" />\n",
)

print("node control route wiring applied")
