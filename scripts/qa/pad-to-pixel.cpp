#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <Xinput.h>
#include <mmsystem.h>
#include <d3d11.h>
#include <dxgi1_2.h>
#include <wrl/client.h>
#include "ViGEmClient.h"
#include <algorithm>
#include <cstdio>
#include <cwchar>
#include <random>
#include <vector>

static bool pressed;
static int frame_id = -1;
static HWND moonlight;

static LRESULT CALLBACK draw(HWND window, UINT message, WPARAM wparam, LPARAM lparam) {
    if (message != WM_PAINT) {
        return DefWindowProcW(window, message, wparam, lparam);
    }
    PAINTSTRUCT paint{};
    HDC dc = BeginPaint(window, &paint);
    RECT rect{};
    GetClientRect(window, &rect);
    COLORREF color = frame_id < 0 ? (pressed ? RGB(0, 220, 0) : RGB(220, 0, 180))
        : RGB(8 + (frame_id & 15) * 16, 8 + ((frame_id >> 4) & 15) * 16, 8 + ((frame_id >> 8) & 15) * 16);
    HBRUSH brush = CreateSolidBrush(color);
    FillRect(dc, &rect, brush);
    DeleteObject(brush);
    EndPaint(window, &paint);
    return 0;
}

static BOOL CALLBACK find_moonlight(HWND window, LPARAM) {
    if (!IsWindowVisible(window)) return TRUE;
    wchar_t cls[128]{};
    GetClassNameW(window, cls, 128);
    if (wcsstr(cls, L"SDL") == nullptr) return TRUE;
    DWORD pid = 0;
    GetWindowThreadProcessId(window, &pid);
    HANDLE process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid);
    if (!process) return TRUE;
    wchar_t image[1024]{};
    DWORD size = 1024;
    bool ok = QueryFullProcessImageNameW(process, 0, image, &size);
    CloseHandle(process);
    if (!ok || wcsstr(image, L"Moonlight.exe") == nullptr) return TRUE;
    RECT rect{};
    GetClientRect(window, &rect);
    if (rect.right < 300 || rect.bottom < 150) return TRUE;
    moonlight = window;
    return FALSE;
}

static int rgb_state(COLORREF color) {
    if (color == CLR_INVALID) return -1;
    if (GetGValue(color) > 145 && GetRValue(color) < 65 && GetBValue(color) < 65) return 1;
    if (GetGValue(color) < 65 && GetRValue(color) > 145 && GetBValue(color) > 95) return 0;
    return -1;
}

using Microsoft::WRL::ComPtr;

class DesktopPixel {
    ComPtr<ID3D11Device> device;
    ComPtr<ID3D11DeviceContext> context;
    ComPtr<IDXGIOutputDuplication> desktop;
    ComPtr<ID3D11Texture2D> pixel;
    COLORREF last = CLR_INVALID;
public:
    HRESULT open() {
        HRESULT hr = D3D11CreateDevice(nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr, 0, nullptr, 0,
            D3D11_SDK_VERSION, &device, nullptr, &context);
        if (FAILED(hr)) return hr;
        ComPtr<IDXGIDevice> dxgi;
        ComPtr<IDXGIAdapter> adapter;
        ComPtr<IDXGIOutput> output;
        ComPtr<IDXGIOutput1> output1;
        if (FAILED(hr = device.As(&dxgi)) || FAILED(hr = dxgi->GetAdapter(&adapter))
            || FAILED(hr = adapter->EnumOutputs(0, &output)) || FAILED(hr = output.As(&output1))
            || FAILED(hr = output1->DuplicateOutput(device.Get(), &desktop))) return hr;
        D3D11_TEXTURE2D_DESC desc{};
        desc.Width = desc.Height = desc.MipLevels = desc.ArraySize = 1;
        desc.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
        desc.SampleDesc.Count = 1;
        desc.Usage = D3D11_USAGE_STAGING;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
        return device->CreateTexture2D(&desc, nullptr, &pixel);
    }

    COLORREF read(int x, int y) {
        DXGI_OUTDUPL_FRAME_INFO info{};
        ComPtr<IDXGIResource> resource;
        HRESULT hr = desktop->AcquireNextFrame(0, &info, &resource);
        if (hr == DXGI_ERROR_WAIT_TIMEOUT) return last;
        if (FAILED(hr)) return CLR_INVALID;
        ComPtr<ID3D11Texture2D> texture;
        hr = resource.As(&texture);
        if (SUCCEEDED(hr)) {
            const UINT left = (UINT)x;
            const UINT top = (UINT)y;
            D3D11_BOX region{left, top, 0, left + 1, top + 1, 1};
            context->CopySubresourceRegion(pixel.Get(), 0, 0, 0, 0, texture.Get(), 0, &region);
            D3D11_MAPPED_SUBRESOURCE data{};
            hr = context->Map(pixel.Get(), 0, D3D11_MAP_READ, 0, &data);
            if (SUCCEEDED(hr)) {
                const auto bgra = static_cast<const BYTE*>(data.pData);
                last = RGB(bgra[2], bgra[1], bgra[0]);
                context->Unmap(pixel.Get(), 0);
            }
        }
        desktop->ReleaseFrame();
        return last;
    }
};

static long long now() {
    LARGE_INTEGER time{};
    QueryPerformanceCounter(&time);
    return time.QuadPart;
}

static DWORD connected() {
    DWORD mask = 0;
    for (DWORD i = 0; i < XUSER_MAX_COUNT; i++) {
        XINPUT_STATE state{};
        if (XInputGetState(i, &state) == ERROR_SUCCESS) mask |= 1 << i;
    }
    return mask;
}

static bool button(DWORD index) {
    XINPUT_STATE state{};
    return XInputGetState(index, &state) == ERROR_SUCCESS && (state.Gamepad.wButtons & XINPUT_GAMEPAD_A);
}

static void wait(DWORD milliseconds) {
    DWORD deadline = GetTickCount() + milliseconds;
    do {
        MSG msg{};
        while (PeekMessageW(&msg, nullptr, 0, 0, PM_REMOVE)) {
            DispatchMessageW(&msg);
        }
        Sleep(1);
    } while ((int)(deadline - GetTickCount()) > 0);
}

int main() {
    SetProcessDPIAware();
    const int width = GetSystemMetrics(SM_CXSCREEN);
    const int height = GetSystemMetrics(SM_CYSCREEN);
    if (width < 1000 || height < 600) return 2;
    HINSTANCE instance = GetModuleHandleW(nullptr);
    WNDCLASSW wc{};
    wc.hInstance = instance;
    wc.lpfnWndProc = draw;
    wc.lpszClassName = L"DuoLinkLatencyMarker";
    RegisterClassW(&wc);
    const int source_x = width / 12;
    const int source_y = height / 5;
    const int source_width = width / 5;
    const int source_height = height / 4;
    HWND source = CreateWindowExW(WS_EX_TOPMOST | WS_EX_NOACTIVATE, wc.lpszClassName,
        L"DuoLink QA", WS_POPUP | WS_VISIBLE, source_x, source_y, source_width, source_height,
        nullptr, nullptr, instance, nullptr);
    if (!source) return 3;

    for (int i = 0; i < 250 && !moonlight; i++) {
        EnumWindows(find_moonlight, 0);
        if (!moonlight) wait(200);
    }
    if (!moonlight) { std::fputs("No Moonlight streaming window\n", stderr); return 4; }
    SetWindowPos(moonlight, HWND_TOP, width * 5 / 12, height / 8, width / 2, height * 2 / 3, SWP_NOACTIVATE);
    wait(1500);
    RECT client{};
    POINT origin{};
    GetClientRect(moonlight, &client);
    ClientToScreen(moonlight, &origin);
    double scale = std::min((double)client.right / width, (double)client.bottom / height);
    int marker_x = origin.x + (int)((client.right - scale * width) / 2 + scale * (source_x + source_width / 2));
    int marker_y = origin.y + (int)((client.bottom - scale * height) / 2 + scale * (source_y + source_height / 2));
    HDC screen = GetDC(nullptr);
    DesktopPixel pixels;
    HRESULT capture = pixels.open();
    auto guest_pixel = [&]() {
        return SUCCEEDED(capture) ? pixels.read(marker_x, marker_y) : GetPixel(screen, marker_x, marker_y);
    };
    COLORREF color = guest_pixel();
    for (int i = 0; i < 30 && rgb_state(color) != 0; i++) { wait(100); color = guest_pixel(); }
    const bool pixel_visible = rgb_state(color) == 0;
    if (!pixel_visible) { std::fprintf(stderr, "Marker is not visible in the guest window: 0x%08lx\n", color); return 5; }
    std::printf("Desktop pixel capture 0x%08lx, source 0x%08lx, guest 0x%08lx at %d,%d\n",
        capture, GetPixel(screen, source_x + 20, source_y + 20), color, marker_x, marker_y);

    DWORD previous = connected();
    PVIGEM_CLIENT feeder = vigem_alloc();
    auto status = vigem_connect(feeder);
    if (!VIGEM_SUCCESS(status)) { std::fprintf(stderr, "vigem_connect 0x%x\n", status); return 6; }
    PVIGEM_TARGET pad = vigem_target_ds4_alloc();
    status = vigem_target_add(feeder, pad);
    if (!VIGEM_SUCCESS(status)) { std::fprintf(stderr, "vigem_target_add 0x%x\n", status); return 7; }
    DS4_REPORT report{};
    DS4_REPORT_INIT(&report);
    const USHORT neutral = report.wButtons;
    vigem_target_ds4_update(feeder, pad, report);
    DWORD created = 0;
    for (int i = 0; i < 120 && !created; i++) {
        wait(100);
        created = connected() & ~previous;
    }
    if (!created || (created & (created - 1))) {
        std::fprintf(stderr, "Expected exactly one new XInput pad, before 0x%lx, after 0x%lx\n", previous, connected());
        return 8;
    }
    DWORD index = 0;
    while (!(created & (1 << index))) index++;
    wait(3000);

    DEVMODEW mode{};
    mode.dmSize = sizeof(mode);
    EnumDisplaySettingsW(nullptr, ENUM_CURRENT_SETTINGS, &mode);
    LARGE_INTEGER frequency{};
    QueryPerformanceFrequency(&frequency);
    double ticks_to_ms = 1000.0 / (double)frequency.QuadPart;
    std::mt19937 random(143);
    std::uniform_int_distribution<DWORD> phase(65, 125);
    std::printf("display=%lux%lu@%lu guest=%ldx%ld host_pad=%lu\n",
        mode.dmPelsWidth, mode.dmPelsHeight, mode.dmDisplayFrequency, client.right, client.bottom, index);
    std::puts("trial,button,guest_to_xinput_ms,guest_to_pixel_ms,driver_submit_ms,local_time");
    timeBeginPeriod(1);
    int failed = 0;
    for (int trial = 0; trial < 88; trial++) {
        wait(phase(random));
        const bool target = (trial & 1) == 0;
        report.wButtons = neutral | (target ? DS4_BUTTON_CROSS : 0);
        SYSTEMTIME clock{};
        GetLocalTime(&clock);
        const auto start = now();
        status = vigem_target_ds4_update(feeder, pad, report);
        const auto submitted = now();
        if (!VIGEM_SUCCESS(status)) { std::fprintf(stderr, "vigem_target_ds4_update 0x%x\n", status); failed++; break; }
        const auto deadline = start + frequency.QuadPart;
        long long source_time = 0;
        while (now() < deadline) {
            if (button(index) == target) { source_time = now(); break; }
            Sleep(1);
        }
        if (!source_time) { std::fprintf(stderr, "XInput did not see event %d\n", trial); failed++; break; }
        pressed = target;
        InvalidateRect(source, nullptr, FALSE);
        UpdateWindow(source);
        long long pixel_time = 0;
        if (pixel_visible) {
            while (now() < deadline) {
                if (rgb_state(guest_pixel()) == (int)target) { pixel_time = now(); break; }
                Sleep(1);
            }
            if (!pixel_time) { std::fprintf(stderr, "Guest did not render event %d\n", trial); failed++; break; }
        }
        if (trial >= 8) {
            std::printf("%d,%s,%.3f,%.3f,%.3f,%02u:%02u:%02u.%03u\n", trial, target ? "down" : "up",
                (source_time - start) * ticks_to_ms, pixel_time ? (pixel_time - start) * ticks_to_ms : 0,
                (submitted - start) * ticks_to_ms, clock.wHour, clock.wMinute, clock.wSecond, clock.wMilliseconds);
        }
    }
    // Software display observation, not panel scanout. A changing 12-bit
    // color marker lets us count delivered frames and gaps without conflating
    // the intentionally sparse gamepad button events with frame pacing.
    if (!failed && pixel_visible) {
        std::vector<long long> presented(4096, 0);
        const auto start = now();
        const auto interval = frequency.QuadPart / 144;
        auto next = start;
        long long last_seen = 0;
        int last_id = -1;
        int invalid = 0;
        while (now() - start < frequency.QuadPart * 12) {
            auto time = now();
            if (time >= next && frame_id < 4094) {
                frame_id++;
                presented[frame_id] = time;
                InvalidateRect(source, nullptr, FALSE);
                UpdateWindow(source);
                next = time + interval;
            }
            COLORREF value = guest_pixel();
            int id = GetRValue(value) / 16 | ((GetGValue(value) / 16) << 4) | ((GetBValue(value) / 16) << 8);
            auto seen = now();
            if (value != CLR_INVALID && id <= frame_id && id > last_id) {
                if (last_seen && seen - start > frequency.QuadPart) {
                    std::printf("frame,%d,%.3f,%.3f\n", id, (seen-last_seen)*ticks_to_ms, (seen-presented[id])*ticks_to_ms);
                }
                last_seen = seen;
                last_id = id;
            } else if (id > frame_id) { invalid++; }
            Sleep(1);
        }
        std::printf("frame_summary,source_frames=%d,last_seen=%d,invalid_reads=%d\n", frame_id+1, last_id, invalid);
    }
    timeEndPeriod(1);
    vigem_target_remove(feeder, pad);
    vigem_target_free(pad);
    vigem_disconnect(feeder);
    vigem_free(feeder);
    ReleaseDC(nullptr, screen);
    DestroyWindow(source);
    return failed ? 9 : 0;
}
