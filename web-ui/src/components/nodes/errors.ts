import { ApiError } from '../../api'
import { nodeErrorMessage } from './errorMessages'
export { nodeErrorMessage }

export function nodeError(cause: unknown): string {
  return cause instanceof ApiError
    ? nodeErrorMessage(cause.code, cause.message)
    : cause instanceof Error
      ? cause.message
      : String(cause)
}
