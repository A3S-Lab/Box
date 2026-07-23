import {
  Sandbox as BaseSandbox,
  type CommandResult,
  type SandboxCreateOptions,
} from './sandbox.js'

export const DEFAULT_CODE_INTERPRETER_IMAGE = 'python:3.12-alpine'

/** Minimal local Python interpreter facade over the native Sandbox API. */
export class Sandbox {
  private constructor(private readonly sandbox: BaseSandbox) {}

  static async create(
    template = DEFAULT_CODE_INTERPRETER_IMAGE,
    options: SandboxCreateOptions = {}
  ): Promise<Sandbox> {
    return new Sandbox(await BaseSandbox.create(template, options))
  }

  get sandboxId(): string {
    return this.sandbox.sandboxId
  }

  get id(): string {
    return this.sandbox.id
  }

  get commands(): BaseSandbox['commands'] {
    return this.sandbox.commands
  }

  get files(): BaseSandbox['files'] {
    return this.sandbox.files
  }

  async runCode(
    code: string,
    options: { language?: 'python'; timeoutMs?: number } = {}
  ): Promise<CommandResult> {
    if (options.language !== undefined && options.language !== 'python') {
      throw new Error('the local Code Interpreter currently supports Python only')
    }
    return this.commands.run(['python', '-c', code], {
      timeoutMs: options.timeoutMs,
    })
  }

  kill(): Promise<void> {
    return this.sandbox.kill()
  }

  pause(options: { keepMemory?: boolean } = {}): Promise<void> {
    return this.sandbox.pause(options)
  }

  resume(): Promise<void> {
    return this.sandbox.resume()
  }

  isRunning(): Promise<boolean> {
    return this.sandbox.isRunning()
  }
}

export default Sandbox
