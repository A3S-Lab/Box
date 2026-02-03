---
name: example-skill
description: Example skill demonstrating all tool backend types
version: 1.0.0
tools:
  # Binary tool - Execute external binary
  - name: echo-tool
    description: Echo a message using the echo binary
    backend:
      type: binary
      path: /bin/echo
      args_template: "${message}"
    parameters:
      type: object
      properties:
        message:
          type: string
          description: Message to echo
      required:
        - message

  # HTTP tool - Make API calls
  - name: weather-api
    description: Get weather information from an API
    backend:
      type: http
      url: https://api.example.com/weather
      method: GET
      headers:
        Authorization: "Bearer ${env:WEATHER_API_KEY}"
    parameters:
      type: object
      properties:
        city:
          type: string
          description: City name
        country:
          type: string
          description: Country code (optional)
      required:
        - city

  # Script tool - Execute Python script
  - name: process-data
    description: Process data with a Python script
    backend:
      type: script
      interpreter: python3
      script: |
        import os
        import json

        # Get arguments from environment
        args = json.loads(os.environ.get('TOOL_ARGS', '{}'))
        data = args.get('data', '')

        # Process the data
        result = data.upper()

        print(f"Processed: {result}")
    parameters:
      type: object
      properties:
        data:
          type: string
          description: Data to process
      required:
        - data

  # Script tool - Bash script
  - name: file-counter
    description: Count files in a directory
    backend:
      type: script
      interpreter: bash
      script: |
        #!/bin/bash
        pattern="${TOOL_ARG_PATTERN:-*}"
        echo "Counting files matching: $pattern"
        find . -name "$pattern" -type f | wc -l
    parameters:
      type: object
      properties:
        pattern:
          type: string
          description: File pattern to match (default: *)
---

# Example Skill

This skill demonstrates all three types of dynamic tools:

## Binary Tools
Execute external binaries with argument templates.

## HTTP Tools
Make HTTP API calls with environment variable substitution for secrets.

## Script Tools
Execute scripts with Python, Bash, or other interpreters.

## Usage

```python
# Activate the skill
box.use_skill("example-skill", skill_content)

# Use the tools
result = box.generate("Use echo-tool to say hello")
result = box.generate("Process 'hello world' with process-data")
result = box.generate("Count all .txt files with file-counter")

# Deactivate the skill
box.remove_skill("example-skill")
```
