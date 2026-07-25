export default {
  async activate(context) {
    context.hooks.on({
      id: "on-turn-end",
      hook: "turn.end",
      mode: "notify",
      handler: async (event) => ({
        action: "continue",
        annotations: { seen: true, hook: event.hook },
      }),
    });
  },
};
