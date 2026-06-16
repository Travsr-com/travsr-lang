// Protocol fixture: used by Dog to verify IsImplementation edges.
#import <Foundation/Foundation.h>

@protocol Speakable <NSObject>
- (NSString *)speak;
- (void)setVolume:(float)level;
@end
