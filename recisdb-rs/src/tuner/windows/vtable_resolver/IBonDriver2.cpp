#include "../IBonDriver.hpp"

extern "C" {
    const LPCTSTR C_EnumTuningSpace(IBonDriver2 * b, const DWORD dwSpace)
    {
        try {
            return b->EnumTuningSpace(dwSpace);
        } catch (...) {
            return nullptr;
        }
    }
    const LPCTSTR C_EnumChannelName2(IBonDriver2 * b, const DWORD dwSpace, const DWORD dwChannel)
    {
        try {
            return b->EnumChannelName(dwSpace, dwChannel);
        } catch (...) {
            return nullptr;
        }
    }
    const BOOL C_SetChannel2(IBonDriver2 * b, const DWORD dwSpace, const DWORD dwChannel)
    {
        try {
            return b->SetChannel(dwSpace, dwChannel);
        } catch (...) {
            return 0;
        }
    }
}
