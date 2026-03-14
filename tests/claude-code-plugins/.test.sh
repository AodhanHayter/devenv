#!/usr/bin/env bash

set -xe

# marketplace.json should be a symlink
test -L .devenv/claude-marketplace/.claude-plugin/marketplace.json

# Plugin dir should be a symlink
test -L .devenv/claude-marketplace/plugins/test-plugin

# Plugin contents should be accessible through symlink
test -f .devenv/claude-marketplace/plugins/test-plugin/.claude-plugin/plugin.json
test -f .devenv/claude-marketplace/plugins/test-plugin/skills/hello/SKILL.md

# settings.json should contain enabledPlugins with the plugin (marketplace name is hashed)
cat .claude/settings.json | jq -e '[.enabledPlugins | to_entries[] | select(.key | startswith("test-plugin@devenv-"))] | length == 1'

# settings.json should contain extraKnownMarketplaces with directory source
cat .claude/settings.json | jq -e '[.extraKnownMarketplaces | to_entries[] | select(.key | startswith("devenv-")) | .value.source.source] | .[0] == "directory"'

# marketplace.json should list the plugin
cat .devenv/claude-marketplace/.claude-plugin/marketplace.json | jq -e '.name | startswith("devenv-")'
cat .devenv/claude-marketplace/.claude-plugin/marketplace.json | jq -e '.plugins[0].name == "test-plugin"'
cat .devenv/claude-marketplace/.claude-plugin/marketplace.json | jq -e '.plugins[0].source == "./plugins/test-plugin"'
cat .devenv/claude-marketplace/.claude-plugin/marketplace.json | jq -e '.owner.name == "devenv"'
