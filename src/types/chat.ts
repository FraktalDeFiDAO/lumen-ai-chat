export type Role = 'user' | 'assistant'
export type Feedback = 'up' | 'down' | null

/// One tool the agent harness ran, from the call to its result.
export interface ToolActivity {
  id: string
  name: string
  arguments?: unknown
  output?: string
  failed?: boolean
}

export interface Message {
  id: string
  role: Role
  content: string
  createdAt: number
  error?: string
  feedback?: Feedback
  /// Present only for answers produced by the agent harness.
  tools?: ToolActivity[]
  /// Records the harness retrieved from the codebase index before answering.
  contextRecords?: number
  /// The harness hit its step limit, so the answer above is incomplete.
  incomplete?: boolean
}

export interface Conversation {
  id: string
  title: string
  messages: Message[]
  createdAt: number
  updatedAt: number
}

export interface ChatModel {
  id: string
  key: string
  name: string
  provider: string
  provider_name: string
  ai_coder: string
  ai_coder_name: string
}
export interface Attachment { id: string; name: string; content: string; size: number }
