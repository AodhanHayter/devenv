{ ... }: {
  claude.code.enable = true;
  claude.code.plugins.test = {
    src = ./mock-plugin;
  };
}
