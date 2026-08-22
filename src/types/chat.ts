export type Role = 'user' | 'assistant'
export type Feedback = 'up' | 'down' | null

export interface Message {
  id: string
  role: Role
  content: string
  createdAt: number
  error?: string
  feedback?: Feedback
}

export interface Conversation {
  id: string
  title: string
  messages: Message[]
  createdAt: number
  updatedAt: number
}

export interface ChatModel { id: string; name: string }
export interface Attachment { id: string; name: string; content: string; size: number }
