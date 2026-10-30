<?php
// Real HTTP SAPI GET registration, with binary-safe typed key/value comparison.
declare(strict_types=1);
function tagged_get(mixed $value): array {
    if (is_array($value)) {
        $entries = [];
        foreach ($value as $key => $child) {
            $tag = is_int($key) ? ['i', (string) $key] : ['s', bin2hex($key)];
            $entries[] = [$tag, tagged_get($child)];
        }
        return ['a', $entries];
    }
    if (!is_string($value)) { throw new RuntimeException('Unexpected GET type'); }
    return ['s', bin2hex($value)];
}
header('Content-Type: application/json');
echo json_encode(['get_tree' => tagged_get($_GET)], JSON_THROW_ON_ERROR), "\n";
