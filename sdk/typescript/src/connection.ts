export interface A3SConnectionEnvironment {
  A3S_BOX_ENDPOINT?: string
  A3S_BOX_DOMAIN?: string
  A3S_BOX_API_KEY?: string
  A3S_BOX_SANDBOX_URL?: string
}

export interface A3SConnectionOptions {
  apiUrl: string
  domain?: string
  apiKey?: string
  sandboxUrl?: string
}

export interface A3SSandboxConnectionOptions {
  apiUrl: string
  domain: string
  validateApiKey: false
  apiKey?: string
  sandboxUrl?: string
}

export interface A3SVolumeConnectionOptions {
  apiUrl: string
}

/** Typed connection values accepted by the pinned official E2B SDK. */
export class A3SConnectionConfig {
  readonly apiUrl: string
  readonly domain: string
  readonly apiKey?: string
  readonly sandboxUrl?: string

  constructor(options: A3SConnectionOptions) {
    if (!options.apiUrl.trim()) throw new Error('apiUrl cannot be empty')
    const derivedDomain = domainFromEndpoint(options.apiUrl)
    const domain = options.domain ?? derivedDomain
    if (!domain.trim()) throw new Error('domain cannot be empty when provided')
    if (options.apiKey !== undefined && !options.apiKey.trim()) {
      throw new Error('apiKey cannot be empty when provided')
    }
    if (options.sandboxUrl !== undefined && !options.sandboxUrl.trim()) {
      throw new Error('sandboxUrl cannot be empty when provided')
    }
    this.apiUrl = options.apiUrl
    this.domain = domain
    this.apiKey = options.apiKey
    this.sandboxUrl = options.sandboxUrl
  }

  static fromEnvironment(
    environment: Readonly<A3SConnectionEnvironment>
  ): A3SConnectionConfig {
    if (!environment.A3S_BOX_ENDPOINT) {
      throw new Error('A3S_BOX_ENDPOINT is required')
    }
    return new A3SConnectionConfig({
      apiUrl: environment.A3S_BOX_ENDPOINT,
      domain: environment.A3S_BOX_DOMAIN,
      apiKey: environment.A3S_BOX_API_KEY,
      sandboxUrl: environment.A3S_BOX_SANDBOX_URL,
    })
  }

  typescriptOptions(): A3SSandboxConnectionOptions {
    return {
      apiUrl: this.apiUrl,
      domain: this.domain,
      validateApiKey: false,
      ...(this.apiKey === undefined ? {} : { apiKey: this.apiKey }),
      ...(this.sandboxUrl === undefined
        ? {}
        : { sandboxUrl: this.sandboxUrl }),
    }
  }

  volumeOptions(): A3SVolumeConnectionOptions {
    return { apiUrl: this.apiUrl }
  }
}

function domainFromEndpoint(endpoint: string): string {
  let url: URL
  try {
    url = new URL(endpoint)
  } catch {
    throw new Error('apiUrl must be an absolute HTTP or HTTPS URL')
  }
  if (url.protocol !== 'http:' && url.protocol !== 'https:') {
    throw new Error('apiUrl must be an absolute HTTP or HTTPS URL')
  }
  return url.hostname.startsWith('api.') ? url.hostname.slice(4) : url.hostname
}
