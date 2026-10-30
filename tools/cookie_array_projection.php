<?php
// PHP 8.4 real HTTP SAPI oracle for scalar and up-to-three-level array cookies.
header('Content-Type: application/json');
function cookie_tagged($value): array {
    if (is_array($value)) {
        $entries = [];
        foreach ($value as $key => $child) {
            $typed_key = is_int($key)
                ? ['i', (string) $key]
                : ['s', bin2hex((string) $key)];
            $entries[] = [$typed_key, cookie_tagged($child)];
        }
        return ['a', $entries];
    }
    return ['s', bin2hex((string) $value)];
}
echo json_encode(['cookie_tree' => cookie_tagged($_COOKIE)], JSON_THROW_ON_ERROR);
