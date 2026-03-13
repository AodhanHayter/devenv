#!/usr/bin/env bash

set -xe

# marketplace.json should be a symlink
test -L .devenv/claude-marketplace/.claude-plugin/marketplace.json

# Plugin dir should be a symlink
test -L .devenv/claude-marketplace/plugins/test-plugin

# Plugin contents should be accessible through symlink
test -f .devenv/claude-marketplace/plugins/test-plugin/.claude-plugin/plugin.json
test -f .devenv/claude-marketplace/plugins/test-plugin/skills/hello/SKILL.md

# settings.json should contain enabledPlugins
cat .claude/settings.json | jq -e '.enabledPlugins["test-plugin@devenv-plugins"] == true'

# settings.json should contain extraKnownMarketplaces
cat .claude/settings.json | jq -e '.extraKnownMarketplaces["devenv-plugins"].source.source == "directory"'

# marketplace.json should list the plugin
cat .devenv/claude-marketplace/.claude-plugin/marketplace.json | jq -e '.plugins[0].name == "test-plugin"'
cat .devenv/claude-marketplace/.claude-plugin/marketplace.json | jq -e '.plugins[0].source == "./plugins/test-plugin"'
cat .devenv/claude-marketplace/.claude-plugin/marketplace.json | jq -e '.owner.name == "devenv"'
