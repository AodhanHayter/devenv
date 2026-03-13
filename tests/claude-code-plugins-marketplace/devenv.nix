{ ... }: {
  claude.code.enable = true;
  # "my-plugin" matches a plugin name in mock-marketplace's marketplace.json.
  # Should resolve to ./plugins/my-plugin without needing subdir.
  claude.code.plugins.my-plugin = {
    src = ./mock-marketplace;
  };
}
