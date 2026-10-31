<?php
// PHP 8.4 real HTTP SAPI oracle for the supported scalar Cookie subset.
header('Content-Type: application/json');
$pairs = [];
foreach ($_COOKIE as $name => $value) {
    if (!is_string($value)) {
        http_response_code(422);
        echo '{"error":"array cookie outside scalar slice"}';
        exit;
    }
    $pairs[] = [bin2hex((string)$name), bin2hex($value)];
}
echo json_encode(['cookie_pairs' => $pairs], JSON_THROW_ON_ERROR);
