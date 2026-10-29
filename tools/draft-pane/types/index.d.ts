export interface Draft {
  target: string
  thread?: string
  text: string
  // What Claude last read of the text, so an edit of the person's reaches it once.
  seen: string
  status: 'editing' | 'fixing' | 'sending' | 'sent' | 'failed'
  // What `slack send` printed: where the message landed, or why it did not.
  outcome?: string
  // The text before the last correction, for Undo fix; any edit drops it.
  typed?: string
  // Why the last correction did not land.
  fixNote?: string
}

declare module 'claude-code' {
  interface PluginState {
    slack: { draft: Shaped<Draft | null> }
  }
}
