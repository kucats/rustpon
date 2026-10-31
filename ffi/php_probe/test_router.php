<?php
declare(strict_types=1);
header('Content-Type: application/json');
if ($_SERVER['REQUEST_METHOD'] === 'GET') {
    echo json_encode(['pid' => getmypid(), 'probe' => extension_loaded('rustpon_probe'),
                      'functions' => function_exists('rustpon_probe_open')],
                     JSON_THROW_ON_ERROR);
    return;
}
$body = file_get_contents('php://input');
if ($body === false || !rustpon_probe_open($body, 'b')) {
    http_response_code(422);
    echo json_encode(['error' => 'open failed']);
    return;
}
$metadata = rustpon_probe_metadata();
$copy = rustpon_probe_copy(0);
if (!is_string($copy) || !is_string($metadata) || $copy !== "ABC\0XYZ") {
    http_response_code(422);
    echo json_encode(['error' => 'metadata or byte copy mismatch']);
    return;
}
$mode = $_GET['mode'] ?? 'normal';
if ($mode === 'multi') {
    if (!rustpon_probe_open($body, 'b') || rustpon_probe_copy(0) !== $copy) {
        http_response_code(422);
        echo json_encode(['error' => 'second open failed']);
        return;
    }
}
if ($mode === 'exit') {
    echo json_encode(['event' => 'exit'], JSON_THROW_ON_ERROR);
    exit;
}
if ($mode === 'uncaught') {
    // The built-in SAPI may have already committed a success status after
    // the JSON content-type header. Make the negative test deterministic.
    http_response_code(500);
    throw new RuntimeException('synthetic test exception after Rust returns');
}
echo json_encode(['event' => 'ok', 'bytes' => strlen($copy),
                  'typed' => str_contains($metadata, '["i","7"]')],
                 JSON_THROW_ON_ERROR);
