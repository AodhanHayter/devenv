#!/usr/bin/env bash

set -xe

# Plugin dir should be resolved via marketplace.json lookup (not subdir)
test -L .devenv/claude-marketplace/plugins/my-plugin

# Plugin contents accessible through symlink
test -f .devenv/claude-marketplace/plugins/my-plugin/.claude-plugin/plugin.json
test -f .devenv/claude-marketplace/plugins/my-plugin/skills/hello/SKILL.md

# settings.json should have the plugin enabled
cat .claude/settings.json | jq -e '.enabledPlugins["my-plugin@devenv-plugins"] == true'

# marketplace.json should list only the enabled plugin, not "other-plugin"
cat .devenv/claude-marketplace/.claude-plugin/marketplace.json | jq -e '.plugins | length == 1'
cat .devenv/claude-marketplace/.claude-plugin/marketplace.json | jq -e '.plugins[0].name == "my-plugin"'

# Verify the source resolved to the correct subdir
cat .devenv/claude-marketplace/.claude-plugin/marketplace.json | jq -e '.plugins[0].source == "./plugins/my-plugin"'
