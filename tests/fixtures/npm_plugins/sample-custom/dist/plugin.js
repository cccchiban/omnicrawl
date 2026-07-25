export default {
  async activate(context) {
    context.hooks.on({
      id: "emit-on-turn-end",
      hook: "turn.end",
      mode: "notify",
      handler: async () => {
        const emit = await context.hooks.emitCustom(
          "plugin.omnicrawl-fixture-sample-custom.batch-flushed",
          { count: 1, label: "fixture" },
          { version: 1 },
        );
        return {
          action: "continue",
          annotations: { emit },
        };
      },
    });

    context.hooks.on({
      id: "on-batch-flushed",
      hook: "plugin.omnicrawl-fixture-sample-custom.batch-flushed",
      mode: "notify",
      handler: async (event) => ({
        action: "continue",
        annotations: { received: event.payload?.count === 1 },
      }),
    });
  },
};
