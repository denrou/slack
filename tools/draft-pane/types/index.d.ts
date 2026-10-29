export interface Draft {
  target: string
  thread?: string
  text: string
  // What Claude last read of the text, so an edit of the person's reaches it once.
  seen: string
  status: 'editing' | 'sending' | 'sent' | 'failed'
  // What `slack send` printed: where the message landed, or why it did not.
  outcome?: string
}

declare module 'claude-code' {
  interface PluginState {
    slack: { draft: Shaped<Draft | null> }
  }
}
