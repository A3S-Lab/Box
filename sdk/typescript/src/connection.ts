export interface A3SRemoteEnvironment {
  A3S_BOX_ENDPOINT?: string
  A3S_BOX_DOMAIN?: string
  A3S_BOX_API_KEY?: string
  A3S_BOX_SANDBOX_URL?: string
}

export interface A3SRemoteConnectionOptions {
  apiUrl: string
  domain?: string
  apiKey?: string
  sandboxUrl?: string
}

export interface OfficialSdkConnectionOptions {
  apiUrl: string
  domain: string
  apiKey?: string
  sandboxUrl?: string
}

/** Explicit configuration for a remote, self-hosted A3S Box service. */
export class A3SRemoteConnection {
  readonly apiUrl: string
  readonly domain: string
  readonly apiKey?: string
  readonly sandboxUrl?: string

  constructor(options: A3SRemoteConnectionOptions) {
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
    environment: Readonly<A3SRemoteEnvironment>
  ): A3SRemoteConnection {
    if (!environment.A3S_BOX_ENDPOINT) {
      throw new Error('A3S_BOX_ENDPOINT is required for remote mode')
    }
    return new A3SRemoteConnection({
      apiUrl: environment.A3S_BOX_ENDPOINT,
      domain: environment.A3S_BOX_DOMAIN,
      apiKey: environment.A3S_BOX_API_KEY,
      sandboxUrl: environment.A3S_BOX_SANDBOX_URL,
    })
  }

  /** Options for an official E2B client used explicitly in remote mode. */
  officialSdkOptions(): OfficialSdkConnectionOptions {
    return {
      apiUrl: this.apiUrl,
      domain: this.domain,
      ...(this.apiKey === undefined ? {} : { apiKey: this.apiKey }),
      ...(this.sandboxUrl === undefined
        ? {}
        : { sandboxUrl: this.sandboxUrl }),
    }
  }

  /** Deprecated alias for officialSdkOptions. */
  typescriptOptions(): OfficialSdkConnectionOptions {
    return this.officialSdkOptions()
  }

  volumeOptions(): { apiUrl: string } {
    return { apiUrl: this.apiUrl }
  }
}

/** Backwards-compatible remote-only class name. */
export { A3SRemoteConnection as A3SConnectionConfig }

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
