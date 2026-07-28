package box

import "strings"

func cloneStringMap(source map[string]string) map[string]string {
	result := make(map[string]string, len(source))
	for key, value := range source {
		result[key] = value
	}
	return result
}

func validateLabels(operation string, labels map[string]string) error {
	for key := range labels {
		if strings.TrimSpace(key) == "" {
			return invalid(operation, "label name cannot be empty")
		}
	}
	return nil
}
