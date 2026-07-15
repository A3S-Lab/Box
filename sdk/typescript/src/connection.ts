export interface A3SConnectionEnvironment {
  E2B_API_URL?: string
  E2B_DOMAIN?: string
  E2B_API_KEY?: string
}

export interface A3SConnectionOptions {
  apiUrl: string
  domain: string
  apiKey?: string
}

/** Typed connection values accepted by the pinned official E2B SDK. */
export class A3SConnectionConfig {
  readonly apiUrl: string
  readonly domain: string
  readonly apiKey?: string

  constructor(options: A3SConnectionOptions) {
    if (!options.apiUrl.trim()) throw new Error('apiUrl cannot be empty')
    if (!options.domain.trim()) throw new Error('domain cannot be empty')
    if (options.apiKey !== undefined && !options.apiKey.trim()) {
      throw new Error('apiKey cannot be empty when provided')
    }
    this.apiUrl = options.apiUrl
    this.domain = options.domain
    this.apiKey = options.apiKey
  }

  static fromEnvironment(
    environment: Readonly<A3SConnectionEnvironment>
  ): A3SConnectionConfig {
    if (!environment.E2B_API_URL) throw new Error('E2B_API_URL is required')
    if (!environment.E2B_DOMAIN) throw new Error('E2B_DOMAIN is required')
    return new A3SConnectionConfig({
      apiUrl: environment.E2B_API_URL,
      domain: environment.E2B_DOMAIN,
      apiKey: environment.E2B_API_KEY,
    })
  }

  typescriptOptions(): A3SConnectionOptions {
    return {
      apiUrl: this.apiUrl,
      domain: this.domain,
      ...(this.apiKey === undefined ? {} : { apiKey: this.apiKey }),
    }
  }
}
