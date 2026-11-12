#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <Xinput.h>
#include "ViGEmClient.h"
#include <cstdio>
#include <vector>
#include <cstdlib>

static DWORD mask() {
    DWORD value = 0;
    for (DWORD i=0;i<4;i++) { XINPUT_STATE state{}; if(XInputGetState(i,&state)==ERROR_SUCCESS) value |= 1<<i; }
    return value;
}
static bool await_mask(DWORD expected) {
    for(int i=0;i<120;i++) { if(mask()==expected) return true; Sleep(50); }
    std::fprintf(stderr,"XInput mask: expected %lx got %lx\n",expected,mask()); return false;
}
static bool await_count(int expected, DWORD excluded, DWORD &found) {
    for(int i=0;i<120;i++) { found=mask() & ~excluded; int count=0;for(int j=0;j<4;j++)count+=(found>>j)&1; if(count==expected)return true;Sleep(50); }
    std::fprintf(stderr,"XInput count mismatch mask %lx excluded %lx expected %d\n",mask(),excluded,expected);return false;
}
static bool sees(WORD buttons, DWORD allowed) {
    for(int retry=0;retry<120;retry++) {
        for(DWORD i=0;i<4;i++) { XINPUT_STATE state{};if((allowed&(1<<i)) && XInputGetState(i,&state)==0 && state.Gamepad.wButtons==buttons)return true; }
        Sleep(25);
    }
    std::fprintf(stderr,"Button %x absent in mask %lx\n",buttons,allowed);return false;
}
static BOOL CALLBACK close_window(HWND w, LPARAM pid) {
    DWORD owner=0;GetWindowThreadProcessId(w,&owner);if(owner==(DWORD)pid && IsWindowVisible(w)) PostMessageW(w,WM_CLOSE,0,0); return TRUE;
}
int main(int argc,char** argv) {
    if(argc!=2)return 2;
    DWORD before=mask();
    if(before) { std::fprintf(stderr,"Physical XInput controllers present: %lx; test needs four free slots\n",before);return 3; }
    PVIGEM_CLIENT client=vigem_alloc();if(!VIGEM_SUCCESS(vigem_connect(client)))return 4;
    std::vector<PVIGEM_TARGET> owned;
    auto add=[&](bool ds4) { auto pad=ds4?vigem_target_ds4_alloc():vigem_target_x360_alloc();if(!VIGEM_SUCCESS(vigem_target_add(client,pad)))std::abort();owned.push_back(pad);if(ds4){DS4_REPORT neutral{};DS4_REPORT_INIT(&neutral);vigem_target_ds4_update(client,pad,neutral);}return pad; };
    auto remove=[&](PVIGEM_TARGET pad) { vigem_target_remove(client,pad); };
    int result=1;
    auto run=[&]() {
        add(false);add(false);DWORD locals=0;
        if(!await_count(2,0,locals))return;
        auto first=add(true);auto second=add(true);DWORD remote=0;
        DS4_REPORT a{},b{};DS4_REPORT_INIT(&a);DS4_REPORT_INIT(&b);
        a.wButtons|=DS4_BUTTON_CROSS;b.wButtons|=DS4_BUTTON_SQUARE;
        for(int i=0;i<30;i++){vigem_target_ds4_update(client,first,a);vigem_target_ds4_update(client,second,b);Sleep(100);}
        if(!await_count(2,locals,remote))return;
        if(!sees(XINPUT_GAMEPAD_A,remote)||!sees(XINPUT_GAMEPAD_X,remote))return;
        std::puts("PASS two local + two forwarded controllers, independent A/X input");
        remove(first);DWORD remaining=0;if(!await_count(1,locals,remaining)||!sees(XINPUT_GAMEPAD_X,remaining))return;
        auto replacement=add(true);
        for(int i=0;i<20;i++){vigem_target_ds4_update(client,replacement,a);Sleep(100);}
        if(!await_count(2,locals,remote))return;
        if(!sees(XINPUT_GAMEPAD_A,remote)||!sees(XINPUT_GAMEPAD_X,remote))return;
        std::puts("PASS controller replacement without leaving stream");
        EnumWindows(close_window,(LPARAM)std::strtoul(argv[1],nullptr,10));
        if(!await_mask(locals))return;
        std::puts("PASS leaving stream released both forwarded XInput controllers");result=0;
    };
    run();
    for(auto pad:owned) { vigem_target_remove(client,pad);vigem_target_free(pad); }
    vigem_disconnect(client);vigem_free(client);
    return result;
}
