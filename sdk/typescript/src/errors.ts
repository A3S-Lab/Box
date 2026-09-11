export class A3SBoxError extends Error {
  readonly code: string
  readonly requestId?: string

  constructor(
    message: string,
    code = 'runtime_error',
    options?: { requestId?: string }
  ) {
    super(message)
    this.name = 'A3SBoxError'
    this.code = code
    if (options?.requestId !== undefined) {
      this.requestId = options.requestId
    }
  }
}

export class A3SBoxNotInstalledError extends A3SBoxError {
  constructor(binary: string) {
    super(
      `Cannot find the local A3S Box executable ${JSON.stringify(binary)}. ` +
        'Install a3s-box or set A3S_BOX_BINARY to its path.',
      'binary_not_found'
    )
    this.name = 'A3SBoxNotInstalledError'
  }
}
