export class A3SBoxError extends Error {
  readonly code: string

  constructor(message: string, code = 'runtime_error') {
    super(message)
    this.name = 'A3SBoxError'
    this.code = code
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
